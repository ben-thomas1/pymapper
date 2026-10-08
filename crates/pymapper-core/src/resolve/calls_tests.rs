use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering as AtomicOrdering};

use super::*;
use crate::index::{Inputs, load};
use crate::roots::Origin;
use crate::walk::{WalkOptions, walk};

/// A fresh temporary directory, removed on drop.
struct TempDir(PathBuf);

impl TempDir {
    fn new() -> TempDir {
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, AtomicOrdering::Relaxed);
        let dir = std::env::temp_dir().join(format!("pymapper-calls-{}-{n}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("create temp dir");
        TempDir(dir)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

struct Cfx {
    ix: Index,
    calls: ResolvedCalls,
    _imp: Imports,
}

/// Loads `files` (relative path, source) as one first-party root and resolves everything.
fn cfx(files: &[(&str, &str)]) -> Cfx {
    let dir = TempDir::new();
    for (rel, src) in files {
        let p = dir.path().join(rel);
        fs::create_dir_all(p.parent().expect("parent")).expect("mkdir");
        fs::write(&p, src).expect("write");
    }
    let w = walk(&[dir.path()], &WalkOptions::everything());
    let ix = load(Inputs::from_root_dir(dir.path(), &w.files, Origin::FirstParty, b"proj"));
    let imp = resolve_imports(&ix, &Attribution::new());
    let calls = resolve_calls(&ix, &imp);
    Cfx { ix, calls, _imp: imp }
}

impl Cfx {
    fn qid(&self, v: NodeId) -> String {
        self.ix.qid_string(v)
    }

    fn node(&self, qid: &str) -> NodeId {
        (0..self.ix.node_count()).find(|&v| self.qid(v) == qid).unwrap_or_else(|| panic!("no node {qid}"))
    }

    fn fmt(&self, e: &CallEdge) -> String {
        format!("{} {} {}", self.qid(e.dst), e.conf.as_str(), e.reason.as_str())
    }

    /// Edges out of `qid`: `dst conf reason`, in edge order.
    fn out(&self, qid: &str) -> Vec<String> {
        let v = self.node(qid);
        self.calls.edges().iter().filter(|e| e.src == v).map(|e| self.fmt(e)).collect()
    }

    /// Every call row of the file of module `module`: `line chain conf reason`.
    fn rows(&self, module: &str) -> Vec<String> {
        let f = (0..self.ix.file_count())
            .find(|&f| self.ix.str(self.ix.file_module(f)) == module.as_bytes())
            .unwrap_or_else(|| panic!("no module {module}"));
        let fv = self.ix.file_view(f);
        let st = self.ix.facts();
        let mut out = Vec::new();
        for row in self.ix.file_facts(f).calls.range() {
            let chain: Vec<String> = fv.call_chain(row).iter().map(|&s| String::from_utf8_lossy(self.ix.str(s)).into_owned()).collect();
            let (c, r) = self.calls.call(row);
            out.push(format!("{} {} {} {}", st.calls.line[row], chain.join("."), c.as_str(), r.as_str()));
        }
        out
    }

    /// The outcome of the only call row of `module` whose chain is `chain`.
    fn row(&self, module: &str, chain: &str) -> String {
        let hits: Vec<String> = self
            .rows(module)
            .into_iter()
            .filter(|r| r.split(' ').nth(1) == Some(chain))
            .map(|r| r.splitn(3, ' ').nth(2).expect("outcome").to_owned())
            .collect();
        assert_eq!(hits.len(), 1, "call {chain} in {module}: {hits:?}");
        hits.into_iter().next().expect("one")
    }

    /// MRO of a class as qids.
    fn mro(&self, qid: &str) -> Vec<String> {
        let v = self.node(qid);
        self.calls.mro_nodes(&self.ix, v).expect("a class").map(|k| self.qid(k)).collect()
    }
}

// --- Ported from the old v2 / integration regressions -------------------------------------------

#[test]
fn from_import_call_is_certain() {
    let fx = cfx(&[("a.py", "from b import foo\n\ndef caller():\n    foo()\n"), ("b.py", "def foo():\n    return 1\n")]);
    assert_eq!(fx.out("a:caller"), ["b:foo certain imported"]);
}

#[test]
fn self_method_resolves_through_base_classes() {
    let src = "class Base:\n    def save(self):\n        pass\n\nclass Child(Base):\n    def run(self):\n        self.save()\n";
    let fx = cfx(&[("mod.py", src)]);
    assert_eq!(fx.out("mod:Child.run"), ["mod:Base.save certain self-method"]);
}

#[test]
fn nested_function_resolves_lexically() {
    let src = "def outer():\n    def inner():\n        return 1\n    return inner()\n\ninner()\n";
    let fx = cfx(&[("mod.py", src)]);
    assert_eq!(fx.out("mod:outer"), ["mod:outer.inner certain local"]);
    // `inner` is not visible at module level.
    assert_eq!(fx.rows("mod"), ["4 inner certain local", "6 inner unresolved not-found"]);
}

#[test]
fn cross_module_fixture() {
    let a = "\
import utils
from b import foo
from b import bar as b_bar
from os.path import join

def local_func():
    pass

def caller():
    foo()
    utils.helper()
    b_bar()
    join('a', 'b')

def same_name_test():
    local_func()
";
    let fx = cfx(&[
        ("a.py", a),
        ("b.py", "def foo():\n    pass\n\ndef bar():\n    pass\n\ndef unused():\n    pass\n"),
        ("utils.py", "def helper():\n    pass\n"),
    ]);
    assert_eq!(fx.out("a:caller"), ["b:foo certain imported", "utils:helper certain module-attr", "b:bar certain imported"]);
    assert_eq!(fx.row("a", "join"), "unresolved external");
    assert_eq!(fx.out("a:same_name_test"), ["a:local_func certain global"]);
    assert!(fx.calls.edges().iter().all(|e| fx.qid(e.dst) != "b:unused"));
}

#[test]
fn scoped_imports_do_not_leak() {
    let src = "\
def helper():
    pass

def a():
    from pkg.m import helper
    helper()

def b():
    helper()
";
    let fx = cfx(&[("mod.py", src), ("pkg/__init__.py", ""), ("pkg/m.py", "def helper():\n    pass\n")]);
    assert_eq!(fx.out("mod:a"), ["pkg.m:helper certain imported"]);
    assert_eq!(fx.out("mod:b"), ["mod:helper certain global"]);
}

#[test]
fn parameters_and_locals_shadow_globals() {
    let src = "\
def helper():
    pass

def by_param(helper):
    helper()

def by_local():
    helper = make()
    helper()

def by_nested_param(helper):
    def inner():
        helper()
    inner()
";
    let fx = cfx(&[("mod.py", src)]);
    assert!(fx.out("mod:by_param").is_empty());
    assert_eq!(fx.rows("mod")[0], "5 helper unresolved parameter");
    assert!(fx.out("mod:by_local").is_empty());
    assert!(fx.out("mod:by_nested_param.inner").is_empty());
    assert_eq!(fx.out("mod:by_nested_param"), ["mod:by_nested_param.inner certain local"]);
}

#[test]
fn class_scope_does_not_enclose_methods() {
    let src = "\
def helper():
    pass

class C:
    def helper(self):
        pass

    def run(self):
        helper()

    value = helper(None)
";
    let fx = cfx(&[("mod.py", src)]);
    assert_eq!(fx.out("mod:C.run"), ["mod:helper certain global"]);
    // Code in the class body itself sees the class scope.
    assert_eq!(fx.out("mod:C"), ["mod:C.helper certain local"]);
}

#[test]
fn global_and_nonlocal_declarations() {
    let src = "\
def helper():
    pass

def outer():
    def helper():
        pass
    def g():
        global helper
        helper()
    def h():
        nonlocal_target()
    def nonlocal_target():
        pass
    g()
";
    let fx = cfx(&[("mod.py", src)]);
    assert_eq!(fx.out("mod:outer.g"), ["mod:helper certain global"]);
    assert_eq!(fx.out("mod:outer.h"), ["mod:outer.nonlocal_target certain local"]);
}

#[test]
fn builtins_and_their_shadowing() {
    let src = "\
def run(xs):
    print(len(xs))
    return sorted(xs)

def open(path):
    pass

def use():
    open('x')
";
    let fx = cfx(&[("mod.py", src)]);
    assert_eq!(fx.row("mod", "print"), "unresolved builtin");
    assert_eq!(fx.row("mod", "len"), "unresolved builtin");
    assert_eq!(fx.out("mod:use"), ["mod:open certain global"]);
}

// --- Classes: instantiation, MRO, self / cls / super() -------------------------------------------

#[test]
fn instantiation_runs_init() {
    let src = "\
class Base:
    def __init__(self):
        pass

class Child(Base):
    pass

class Plain:
    pass

class WithNew:
    def __new__(cls):
        pass

def make():
    Child()
    Plain()
    WithNew()
";
    let fx = cfx(&[("mod.py", src)]);
    assert_eq!(
        fx.out("mod:make"),
        [
            "mod:Child certain constructor",
            "mod:Base.__init__ certain init",
            "mod:Plain certain constructor",
            "mod:WithNew certain constructor",
            "mod:WithNew.__new__ certain init",
        ]
    );
    assert_eq!(fx.row("mod", "Child"), "certain constructor");
}

#[test]
fn c3_mro_and_diamond_lookup() {
    let src = "\
class A:
    def m(self):
        pass

class B(A):
    pass

class C(A):
    def m(self):
        pass

class D(B, C):
    def run(self):
        self.m()
";
    let fx = cfx(&[("mod.py", src)]);
    assert_eq!(fx.mro("mod:D"), ["mod:D", "mod:B", "mod:C", "mod:A"]);
    // C3 finds C.m before A.m (a depth-first search would find A.m through B).
    assert_eq!(fx.out("mod:D.run"), ["mod:C.m certain self-method"]);
}

#[test]
fn c3_classic_example() {
    // The example of the Python 2.3 MRO paper.
    let src = "\
class O: pass
class F(O): pass
class E(O): pass
class D(O): pass
class C(D, F): pass
class B(D, E): pass
class A(B, C): pass
";
    let fx = cfx(&[("mod.py", src)]);
    assert_eq!(fx.mro("mod:A"), ["mod:A", "mod:B", "mod:C", "mod:D", "mod:E", "mod:F", "mod:O"]);
    let a = fx.calls.classes().id(&fx.ix, fx.node("mod:A")).expect("class");
    assert_eq!(fx.calls.classes().flags(a), 0);
}

#[test]
fn inconsistent_and_cyclic_bases_do_not_panic() {
    let src = "\
class X: pass
class Y(X): pass
class Z(X, Y): pass

class P(Q): pass
class Q(P): pass
";
    let fx = cfx(&[("mod.py", src)]);
    let cl = fx.calls.classes();
    let z = cl.id(&fx.ix, fx.node("mod:Z")).expect("class");
    assert_ne!(cl.flags(z) & class_flags::NO_C3, 0);
    assert_eq!(fx.mro("mod:Z")[0], "mod:Z");
    let p = cl.id(&fx.ix, fx.node("mod:P")).expect("class");
    let q = cl.id(&fx.ix, fx.node("mod:Q")).expect("class");
    assert_ne!((cl.flags(p) | cl.flags(q)) & class_flags::NO_C3, 0);
}

#[test]
fn self_referencing_base_is_incomplete() {
    let src = "from base import Thing\n\nclass Thing(Thing):\n    def go(self):\n        self.other()\n";
    let fx = cfx(&[("mod.py", src), ("base.py", "class Thing:\n    def other(self):\n        pass\n")]);
    // The module-level `Thing` is the class itself; its base is not followed.
    assert_eq!(fx.row("mod", "self.other"), "unresolved external-base");
}

#[test]
fn super_and_cls_calls() {
    let src = "\
import threading

class Base:
    def __init__(self):
        pass

    @classmethod
    def create(cls):
        return cls.build()

    @classmethod
    def build(cls):
        pass

class Child(Base):
    def __init__(self):
        super().__init__()
        super(Child, self).__init__()

class Worker(threading.Thread):
    def __init__(self):
        super().__init__()
        self.start()

def plain():
    self.go()
";
    let fx = cfx(&[("mod.py", src)]);
    assert_eq!(fx.out("mod:Base.create"), ["mod:Base.build certain cls-method"]);
    assert_eq!(fx.out("mod:Child.__init__"), ["mod:Base.__init__ certain super-method", "mod:Base.__init__ certain super-method"]);
    let rows = fx.rows("mod");
    assert!(rows.contains(&"22 __init__ unresolved external-base".to_owned()), "{rows:?}");
    assert!(rows.contains(&"23 self.start unresolved external-base".to_owned()), "{rows:?}");
    assert_eq!(fx.row("mod", "self.go"), "unresolved no-class");
}

#[test]
fn generic_and_protocol_bases_keep_the_mro_complete() {
    let src = "\
from typing import Generic, Protocol, TypeVar
from abc import ABC

T = TypeVar('T')

class Box(Generic[T], ABC):
    def get(self):
        self.missing()

class P(Protocol):
    pass
";
    let fx = cfx(&[("mod.py", src)]);
    let cl = fx.calls.classes();
    for q in ["mod:Box", "mod:P"] {
        let c = cl.id(&fx.ix, fx.node(q)).expect("class");
        assert_eq!(cl.flags(c) & class_flags::INCOMPLETE, 0, "{q}");
    }
    assert_eq!(fx.row("mod", "self.missing"), "unresolved missing-attr");
}

#[test]
fn unloaded_bases_propagate_through_the_mro() {
    let src = "\
class Err(Exception):
    def detail(self):
        pass

class Sub(Err):
    def go(self):
        self.detail()
        self.add_note('x')

class Dyn:
    def __getattr__(self, name):
        pass

class SubDyn(Dyn):
    def go(self):
        self.anything()
";
    let fx = cfx(&[("mod.py", src)]);
    let cl = fx.calls.classes();
    let sub = cl.id(&fx.ix, fx.node("mod:Sub")).expect("class");
    assert_ne!(cl.flags(sub) & class_flags::INCOMPLETE, 0);
    // Found in a base after an unloaded one could come first: only likely.
    assert_eq!(fx.out("mod:Sub.go"), ["mod:Err.detail likely self-method"]);
    assert_eq!(fx.row("mod", "self.add_note"), "unresolved external-base");
    assert_eq!(fx.row("mod", "self.anything"), "unresolved dynamic");
}

#[test]
fn subclass_methods_of_self_calls() {
    let src = "\
class Base:
    def run(self):
        self.hook()

class A(Base):
    def hook(self):
        pass

class B(Base):
    def hook(self):
        pass

class Other:
    def hook(self):
        pass
";
    let fx = cfx(&[("mod.py", src)]);
    assert_eq!(fx.out("mod:Base.run"), ["mod:A.hook likely subclass-method", "mod:B.hook likely subclass-method"]);
    assert_eq!(fx.row("mod", "self.hook"), "likely subclass-method");
}

#[test]
fn override_liveness_edges() {
    let src = "\
class A:
    def m(self):
        pass

class B(A):
    def m(self):
        pass

class C(A):
    def m(self):
        pass

class D(B, C):
    def m(self):
        pass

class E(D):
    def other(self):
        pass
";
    let fx = cfx(&[("mod.py", src)]);
    assert_eq!(fx.out("mod:A.m"), ["mod:B.m likely override", "mod:C.m likely override"]);
    // Diamond: D.m overrides B.m and C.m; A.m reaches it through them.
    assert_eq!(fx.out("mod:B.m"), ["mod:D.m likely override"]);
    assert_eq!(fx.out("mod:C.m"), ["mod:D.m likely override"]);
    let overrides = fx.calls.edges().iter().filter(|e| e.reason == Reason::Override).count();
    assert_eq!(overrides, 4);
    assert!(fx.calls.edges().iter().all(|e| e.reason != Reason::Override || e.site == crate::index::NONE));
}

#[test]
fn class_attributes_and_nested_classes() {
    let src = "\
class Outer:
    class Inner:
        def __init__(self):
            pass

    def _handle(self):
        pass

    handler = _handle

    @staticmethod
    def util():
        pass

    def run(self):
        self.handler()

def use():
    Outer.Inner()
    Outer.util()
";
    let fx = cfx(&[("mod.py", src)]);
    assert_eq!(
        fx.out("mod:use"),
        ["mod:Outer.Inner certain constructor", "mod:Outer.Inner.__init__ certain init", "mod:Outer.util certain class-attr"]
    );
    assert_eq!(fx.out("mod:Outer.run"), ["mod:Outer._handle likely self-method"]);
}

// --- Hints, aliases and fallbacks ---------------------------------------------------------------

#[test]
fn construction_hints() {
    let app = "\
class App:
    def run(self):
        pass

    def __call__(self):
        pass

app = App()
";
    let main = "\
from app import App, app
import app as appmod

class Db:
    def query(self):
        pass

class Service:
    def __init__(self):
        self.db = Db()
        self.app = None

    def work(self):
        self.db.query()

def local():
    x = App()
    x.run()
    x()

def via_global():
    app.run()
    appmod.app.run()

def mixed(flag):
    y = App()
    if flag:
        y = Db()
    y.run()
";
    let fx = cfx(&[("app.py", app), ("main.py", main)]);
    assert_eq!(
        fx.out("main:local"),
        ["app:App certain constructor", "app:App.run likely local-hint", "app:App.__call__ likely call-dunder"]
    );
    assert_eq!(fx.out("main:via_global"), ["app:App.run likely global-hint", "app:App.run likely global-hint"]);
    assert_eq!(fx.out("main:Service.work"), ["main:Db.query likely attr-hint"]);
    // Two different constructions of `y` in one scope: one level less sure.
    assert_eq!(fx.out("main:mixed")[2], "app:App.run possible local-hint");
}

#[test]
fn aliases() {
    let src = "\
import helpers

def helper():
    pass

run = helper
other = helpers.util

def use():
    run()
    other()
";
    let fx = cfx(&[("mod.py", src), ("helpers.py", "def util():\n    pass\n")]);
    assert_eq!(fx.out("mod:use"), ["mod:helper likely alias", "helpers:util likely alias"]);
}

#[test]
fn unique_method_fallback() {
    let src = "\
class Frob:
    def frobnicate(self):
        pass

    def append(self, x):
        pass

class One:
    def shared(self):
        pass

class Two:
    def shared(self):
        pass

def use(obj, items):
    obj.frobnicate()
    obj.shared()
    items.append(1)
    get_thing().frobnicate()
    obj.nothing_like_this()
";
    let fx = cfx(&[("mod.py", src)]);
    assert_eq!(fx.out("mod:use"), ["mod:Frob.frobnicate possible unique-method", "mod:Frob.frobnicate possible unique-method"]);
    assert_eq!(fx.row("mod", "obj.shared"), "unresolved ambiguous");
    assert_eq!(fx.row("mod", "items.append"), "unresolved builtin-method");
    assert_eq!(fx.row("mod", "obj.nothing_like_this"), "unresolved parameter");
    assert_eq!(fx.row("mod", "get_thing"), "unresolved not-found");
}

#[test]
fn decorators_are_calls() {
    let src = "\
from registry import register, route

def deco(fn):
    return fn

class Marker:
    def __init__(self, fn):
        pass

@deco
def a():
    pass

@register
@Marker
def b():
    pass

@route('/x')
def c():
    pass

class K:
    @deco
    def m(self):
        pass

    @property
    def p(self):
        pass
";
    let reg = "def register(fn):\n    return fn\n\ndef route(path):\n    pass\n";
    let fx = cfx(&[("mod.py", src), ("registry.py", reg)]);
    assert_eq!(
        fx.out("mod"),
        [
            "registry:route certain imported",
            "mod:deco certain decorator",
            "registry:register certain decorator",
            "mod:Marker certain constructor",
            "mod:Marker.__init__ certain init",
        ]
    );
    // Decorators in a class body are evaluated there.
    assert_eq!(fx.out("mod:K"), ["mod:deco certain decorator"]);
}

#[test]
fn module_attribute_chains_and_star_imports() {
    let fx = cfx(&[
        ("main.py", "import pkg.sub.mod\nfrom star import *\n\ndef use():\n    pkg.sub.mod.f()\n    pkg.sub.mod.K.g()\n    s()\n    pkg.sub.missing()\n    pkg()\n"),
        ("pkg/__init__.py", ""),
        ("pkg/sub/__init__.py", ""),
        ("pkg/sub/mod.py", "def f():\n    pass\n\nclass K:\n    @staticmethod\n    def g():\n        pass\n"),
        ("star.py", "def s():\n    pass\n"),
    ]);
    assert_eq!(fx.out("main:use"), ["pkg.sub.mod:f certain module-attr", "pkg.sub.mod:K.g certain class-attr", "star:s certain imported"]);
    assert_eq!(fx.row("main", "pkg.sub.missing"), "unresolved missing-attr");
    assert_eq!(fx.row("main", "pkg"), "unresolved not-callable");
}

#[test]
fn namespace_package_attributes() {
    let fx = cfx(&[("main.py", "import ns.mod\n\ndef use():\n    ns.mod.f()\n"), ("ns/mod.py", "def f():\n    pass\n")]);
    assert_eq!(fx.out("main:use"), ["ns.mod:f certain module-attr"]);
}

#[test]
fn module_level_callers_and_lambdas() {
    let src = "\
def helper():
    pass

helper()
cb = lambda: helper()

if __name__ == '__main__':
    helper()
";
    let fx = cfx(&[("mod.py", src)]);
    assert_eq!(fx.out("mod"), ["mod:helper certain global", "mod:helper likely global", "mod:helper certain global"]);
}

#[test]
fn local_import_of_a_module_and_reexported_variables() {
    let fx = cfx(&[
        ("main.py", "def use():\n    import tools\n    tools.go()\n    from ext import db\n    db.session()\n"),
        ("tools.py", "def go():\n    pass\n"),
        ("ext.py", "from models import Database\n\ndb = Database()\n"),
        ("models.py", "class Database:\n    def session(self):\n        pass\n"),
    ]);
    assert_eq!(fx.out("main:use"), ["tools:go certain module-attr", "models:Database.session likely global-hint"]);
}

#[test]
fn outcomes_cover_every_call_row_and_are_deterministic() {
    let src = "\
import os

class A:
    def __init__(self):
        self.x = B()

    def go(self):
        self.x.run()
        os.getcwd()

class B:
    def run(self):
        A().go()
";
    // Overrides in many files: class members are sorted by `Sym`, whose values depend on the
    // thread count, so this checks that override edges do not follow member order.
    let mut owned: Vec<(String, String)> = vec![
        ("m.py".to_owned(), src.to_owned()),
        ("n.py".to_owned(), "from m import A, B\n\ndef f(x):\n    A().go()\n    x.run()\n    B.run(None)\n".to_owned()),
    ];
    for i in 0..24 {
        let body = format!(
            "class A{i}:\n    def zeta{i}(self): pass\n    def alpha{i}(self): pass\n    def mid{i}(self): pass\n\n\
             class B{i}(A{i}):\n    def zeta{i}(self): pass\n    def alpha{i}(self): pass\n    def mid{i}(self): pass\n"
        );
        owned.push((format!("o{i}.py"), body));
    }
    owned.push((
        "p.py".to_owned(),
        "from o0 import A0, B0\n\nclass C(B0):\n    def mid0(self): pass\n    def zeta0(self): pass\n".to_owned(),
    ));
    let files: Vec<(&str, &str)> = owned.iter().map(|(a, b)| (a.as_str(), b.as_str())).collect();
    let fx = cfx(&files);
    assert_eq!(fx.out("o0:A0.zeta0"), ["o0:B0.zeta0 likely override"]);
    let overrides: Vec<String> =
        fx.calls.edges().iter().filter(|e| e.reason == Reason::Override).map(|e| fx.qid(e.dst)).collect();
    let p0 = overrides.iter().position(|q| q == "o0:B0.zeta0").expect("override");
    assert_eq!(overrides[p0..p0 + 3], ["o0:B0.zeta0", "o0:B0.alpha0", "o0:B0.mid0"]);
    let c0 = overrides.iter().position(|q| q == "p:C.mid0").expect("override");
    assert_eq!(overrides[c0..c0 + 2], ["p:C.mid0", "p:C.zeta0"]);
    assert_eq!(fx.calls.len(), fx.ix.facts().calls.len() as usize);
    let s = fx.calls.stats();
    assert_eq!(s.calls, fx.calls.len() as u64);
    assert_eq!(s.by_conf.iter().sum::<u64>(), s.calls);
    assert_eq!(s.edges, fx.calls.edges().len() as u64);
    let mut total = 0;
    for f in 0..fx.ix.file_count() {
        total += fx.calls.file_edges(f).len();
    }
    assert_eq!(total, fx.calls.edges().len());
    for threads in [1, 2, 3, 7] {
        let pool = rayon::ThreadPoolBuilder::new().num_threads(threads).build().expect("pool");
        let other = pool.install(|| cfx(&files));
        let a: Vec<String> = fx.calls.edges().iter().map(|e| format!("{} {}", fx.qid(e.src), fx.fmt(e))).collect();
        let b: Vec<String> = other.calls.edges().iter().map(|e| format!("{} {}", other.qid(e.src), other.fmt(e))).collect();
        assert_eq!(a, b);
        assert_eq!(fx.rows("m"), other.rows("m"));
        assert_eq!(fx.rows("n"), other.rows("n"));
    }
    assert!(fx.calls.heap_bytes() > 0);
}

#[test]
fn object_attributes_metaclasses_and_unknown_heads() {
    let src = "\
from abc import ABCMeta

class Base:
    def copy(self):
        return self.__class__()

class Child(Base):
    def __init__(self):
        super().__init__()
        self.widget.configure()
        self.widget.only_here()

class Meta(metaclass=ABCMeta):
    pass

class Other:
    def only_here(self):
        pass

def use(ks):
    Meta.register(int)
    return [k.lower() for k in ks] + [k.only_here() for k in ks]
";
    let fx = cfx(&[("mod.py", src), ("star.py", "from _unloaded import *\n\nlen([])\nunknown_name()\n")]);
    assert_eq!(fx.out("mod:Base.copy"), ["mod:Base likely constructor"]);
    assert_eq!(fx.row("mod", "__init__"), "unresolved builtin");
    // `self.widget` is set elsewhere: the receiver is unknown, so the fallback applies.
    assert_eq!(fx.row("mod", "self.widget.configure"), "unresolved variable");
    assert_eq!(fx.row("mod", "self.widget.only_here"), "possible unique-method");
    assert_eq!(fx.row("mod", "Meta.register"), "unresolved external-base");
    // A star import of unloaded code does not hide the builtins (but may bind other names).
    assert_eq!(fx.row("star", "len"), "unresolved builtin");
    assert_eq!(fx.row("star", "unknown_name"), "unresolved dynamic");
    assert_eq!(fx.row("mod", "k.lower"), "unresolved builtin-method");
    assert_eq!(fx.row("mod", "k.only_here"), "possible unique-method");
}

/// The stdlib as one root (`PYMAPPER_STDLIB` overrides the path). Prints the outcome shares;
/// `PYMAPPER_DEBUG_CALLS=<reason>` also prints the most common chains with that reason.
#[test]
#[ignore = "needs a local CPython stdlib"]
fn stdlib_calls_resolve() {
    let root = std::env::var_os("PYMAPPER_STDLIB").map(PathBuf::from).unwrap_or_else(|| {
        PathBuf::from("/Users/bt386671/.local/share/uv/python/cpython-3.13.5-macos-aarch64-none/lib/python3.13")
    });
    if !root.is_dir() {
        eprintln!("skipping: {} not found", root.display());
        return;
    }
    let w = walk(&[root.as_path()], &WalkOptions::everything());
    let ix = load(Inputs::from_root_dir(&root, &w.files, Origin::Stdlib, b"stdlib"));
    let mut attr = Attribution::new();
    attr.add_stdlib_dir(&root);
    let imp = resolve_imports(&ix, &attr);
    let calls = resolve_calls(&ix, &imp);
    let st = calls.stats();
    eprintln!("calls {} edges {} classes {} incomplete {} no-c3 {}", st.calls, st.edges, st.classes, st.incomplete_classes, st.no_c3);
    for c in 0..4u8 {
        eprintln!("  {:<10} {:>6.1}%", Confidence::from_u8(c).as_str(), st.by_conf[c as usize] as f64 * 100.0 / st.calls as f64);
    }
    for r in 0..Reason::COUNT as u8 {
        let n = st.by_reason[r as usize];
        if n > 0 {
            eprintln!("  {:<18} {:>6.1}%  edges {}", Reason::from_u8(r).as_str(), n as f64 * 100.0 / st.calls as f64, st.edges_by_reason[r as usize]);
        }
    }
    if let Some(only) = std::env::var_os("PYMAPPER_DEBUG_CALLS") {
        let only = only.to_string_lossy().into_owned();
        let mut hist: std::collections::HashMap<String, u32> = std::collections::HashMap::new();
        for f in 0..ix.file_count() {
            let fv = ix.file_view(f);
            for row in ix.file_facts(f).calls.range() {
                if calls.call(row).1.as_str() != only {
                    continue;
                }
                let chain: Vec<String> = fv.call_chain(row).iter().map(|&s| String::from_utf8_lossy(ix.str(s)).into_owned()).collect();
                let recv = format!("{:?}", ix.facts().calls.receiver(row));
                *hist.entry(format!("{recv} {}", chain.join("."))).or_default() += 1;
            }
        }
        let mut v: Vec<(String, u32)> = hist.into_iter().collect();
        v.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        for (k, n) in v.iter().take(40) {
            eprintln!("  {n:>6} {k}");
        }
    }
    assert!(st.by_conf[Confidence::Certain as usize] > 0);
}

#[test]
fn cls_call_in_a_classmethod_is_an_instantiation() {
    let src = "\
class UsesCls:
    def __init__(self, x=0):
        self.x = x

    @classmethod
    def make(cls):
        return cls()

    @classmethod
    def later(cls):
        def inner():
            return cls(1)
        return inner

    @classmethod
    def rebound(cls, other):
        cls = other
        return cls()

    @classmethod
    def factory(cls, kind):
        return kind()

    @staticmethod
    def static(cls):
        return cls()

    def plain(cls):
        return cls()

class Sub(UsesCls):
    @classmethod
    def build(cls):
        return cls()

def free(cls):
    return cls()
";
    let fx = cfx(&[("mod.py", src)]);
    let init = ["mod:UsesCls likely constructor", "mod:UsesCls.__init__ likely init"];
    assert_eq!(fx.out("mod:UsesCls.make"), init);
    assert_eq!(fx.out("mod:UsesCls.later.inner"), init);
    // A subclass: its own constructor and the inherited `__init__`.
    assert_eq!(fx.out("mod:Sub.build"), ["mod:Sub likely constructor", "mod:UsesCls.__init__ likely init"]);
    assert!(fx.out("mod:UsesCls.rebound").is_empty());
    assert_eq!(fx.row("mod", "kind"), "unresolved parameter");
    // Not a classmethod, or not in a class: `cls` is any parameter.
    for scope in ["mod:UsesCls.static", "mod:UsesCls.plain", "mod:free"] {
        assert!(fx.out(scope).is_empty(), "{scope}");
    }
    let params = fx.rows("mod").iter().filter(|r| r.ends_with(" cls unresolved parameter")).count();
    assert_eq!(params, 3, "{:?}", fx.rows("mod"));
}
