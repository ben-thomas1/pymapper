use std::fmt::Write as _;
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
    fn new(tag: &str) -> TempDir {
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, AtomicOrdering::Relaxed);
        let dir = std::env::temp_dir().join(format!("pymapper-resolve-{tag}-{}-{n}", std::process::id()));
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

struct Fx {
    ix: Index,
    imp: Imports,
}

fn fixture(root: &Path) -> Fx {
    fixture_with(root, &Attribution::new())
}

fn fixture_with(root: &Path, attr: &Attribution) -> Fx {
    let ix = load_dir(root);
    let imp = resolve_imports(&ix, attr);
    Fx { ix, imp }
}

fn flag_names(flags: u8) -> String {
    let names = [
        (res_flags::TYPE_ONLY, "type"),
        (res_flags::CONDITIONAL, "cond"),
        (res_flags::TRY, "try"),
        (res_flags::LAZY, "lazy"),
        (res_flags::STAR, "star"),
        (res_flags::EXPORTED, "exported"),
        (res_flags::VIA_REEXPORT, "reexport"),
        (res_flags::VIA_STAR, "viastar"),
    ];
    let v: Vec<&str> = names.iter().filter(|(f, _)| flags & f != 0).map(|&(_, n)| n).collect();
    v.join(",")
}

impl Fx {
    fn file(&self, module: &str) -> FileId {
        (0..self.ix.file_count())
            .find(|&f| self.ix.str(self.ix.file_module(f)) == module.as_bytes() && !is_pyi(&self.ix, f))
            .or_else(|| (0..self.ix.file_count()).find(|&f| self.ix.str(self.ix.file_module(f)) == module.as_bytes()))
            .unwrap_or_else(|| panic!("no file for module {module}"))
    }

    fn qid(&self, v: NodeId) -> String {
        self.ix.qid_string(v)
    }

    /// One line per import row of a file: `line status target [flags]`.
    fn rows(&self, module: &str) -> Vec<String> {
        let f = self.file(module);
        let mut out = Vec::new();
        for row in self.ix.file_facts(f).imports.range() {
            out.push(self.row(row));
        }
        out
    }

    fn row(&self, row: usize) -> String {
        let line = self.ix.facts().imports.line[row];
        let st = self.imp.status(row);
        let mut s = format!("{line} {}", st.as_str());
        match st {
            ImportStatus::Stdlib | ImportStatus::External => {
                let _ = write!(s, " {}", String::from_utf8_lossy(self.imp.ext_name(&self.ix, row).unwrap_or(b"?")));
            }
            ImportStatus::ThirdParty => {
                let _ = write!(s, " dist{}", self.imp.dist(row).map_or(u32::MAX, |d| d.0));
            }
            _ => {
                if let Some(t) = self.imp.target(row) {
                    let _ = write!(s, " {}", self.qid(t));
                } else if let Some(m) = self.imp.module(row).and_then(|m| self.imp.modules().node(m)) {
                    let _ = write!(s, " in {}", self.qid(m));
                }
            }
        }
        let fl = flag_names(self.imp.flags(row));
        if !fl.is_empty() {
            let _ = write!(s, " [{fl}]");
        }
        s
    }

    /// Module edges of a file: `dst rows [flags]`.
    fn edges(&self, module: &str) -> Vec<String> {
        let f = self.file(module);
        self.imp
            .module_edges(f)
            .iter()
            .map(|e| {
                let fl = flag_names(e.flags);
                let mut s = format!("{} x{}", self.qid(e.dst), e.rows);
                if !fl.is_empty() {
                    let _ = write!(s, " [{fl}]");
                }
                s
            })
            .collect()
    }

    fn ext_edges(&self, module: &str) -> Vec<String> {
        let f = self.file(module);
        self.imp
            .ext_edges(f)
            .iter()
            .map(|e| match e.kind {
                ExtKind::ThirdParty => format!("dist{} x{}", e.key, e.rows),
                k => format!("{k:?} {} x{}", String::from_utf8_lossy(edges::ext_name(&self.ix, e.key)), e.rows),
            })
            .collect()
    }

    fn sym(&self, s: &str) -> Sym {
        self.ix.strings().get(s.as_bytes()).unwrap_or_else(|| panic!("{s} not interned"))
    }

    fn global(&self, module: &str, name: &str) -> Option<String> {
        let g = self.imp.global(&self.ix, self.file(module), self.sym(name))?;
        Some(self.global_str(g))
    }

    fn global_str(&self, g: Global) -> String {
        match g {
            Global::Def(v) => format!("def {}", self.qid(v)),
            Global::Module(v) => format!("module {}", self.qid(v)),
            Global::Variable(v) => format!("var {}", self.qid(v)),
            Global::Unresolved(r) => format!("unresolved {}", self.row(r as usize)),
            Global::Dynamic => "dynamic".to_owned(),
        }
    }
}

fn is_pyi(ix: &Index, f: FileId) -> bool {
    ix.file_path(f).extension().is_some_and(|e| e == "pyi")
}

fn s(v: &[&str]) -> Vec<String> {
    v.iter().map(|x| (*x).to_owned()).collect()
}

#[test]
fn absolute_imports_and_names() {
    let t = TempDir::new("abs");
    t.file("pkg/__init__.py", "")
        .file("pkg/mod.py", "def f():\n    pass\nclass C:\n    pass\nX = 1\n")
        .file("pkg/sub/__init__.py", "")
        .file("pkg/sub/deep.py", "")
        .file(
            "main.py",
            "import pkg.mod\nimport pkg.sub.deep as d\nfrom pkg.mod import f, C, X\nfrom pkg import sub\nfrom pkg.mod import nope\nimport pkg.nothere\n",
        );
    let fx = fixture(t.path());
    assert_eq!(
        fx.rows("main"),
        s(&[
            "1 module pkg.mod",
            "2 module pkg.sub.deep",
            "3 symbol pkg.mod:f",
            "3 symbol pkg.mod:C",
            "3 variable pkg.mod",
            "4 module pkg.sub",
            "5 missing-name in pkg.mod",
            "6 missing-module in pkg",
        ])
    );
    // `import pkg.mod` binds `pkg`; `import ... as d` binds the full module.
    assert_eq!(fx.global("main", "pkg").as_deref(), Some("module pkg"));
    assert_eq!(fx.global("main", "d").as_deref(), Some("module pkg.sub.deep"));
    assert_eq!(fx.global("main", "C").as_deref(), Some("def pkg.mod:C"));
    assert_eq!(fx.edges("main"), s(&["pkg x1", "pkg.mod x5", "pkg.sub x1", "pkg.sub.deep x1"]));
}

#[test]
fn relative_imports() {
    let t = TempDir::new("rel");
    t.file("pkg/__init__.py", "from . import a\nfrom .a import fa\n")
        .file("pkg/a.py", "def fa():\n    pass\n")
        .file("pkg/sub/__init__.py", "from .. import a\n")
        .file("pkg/sub/b.py", "from . import c\nfrom ..a import fa\nfrom ...x import y\nfrom .c import fc\nfrom .missing import z\n")
        .file("pkg/sub/c.py", "def fc():\n    pass\n")
        .file("top.py", "from . import a\n");
    let fx = fixture(t.path());
    assert_eq!(fx.rows("pkg"), s(&["1 module pkg.a [exported]", "2 symbol pkg.a:fa [exported]"]));
    // `pkg` binds `a` through its own `from . import a`.
    assert_eq!(fx.rows("pkg.sub"), s(&["1 module pkg.a [exported,reexport]"]));
    assert_eq!(
        fx.rows("pkg.sub.b"),
        s(&[
            "1 module pkg.sub.c",
            "2 symbol pkg.a:fa",
            "3 beyond-top",
            "4 symbol pkg.sub.c:fc",
            "5 missing-module in pkg.sub",
        ])
    );
    // A top-level module has no package.
    assert_eq!(fx.rows("top"), s(&["1 beyond-top"]));
}

#[test]
fn reexports_through_init() {
    let t = TempDir::new("reexport");
    t.file(
        "pkg/__init__.py",
        "from ._impl import Thing, helper as public_helper\nfrom pkg._impl import CONST as CONST\nimport pkg.other\n",
    )
    .file("pkg/_impl.py", "class Thing:\n    pass\ndef helper():\n    pass\nCONST = 3\n")
    .file("pkg/other.py", "")
    .file("user.py", "from pkg import Thing, public_helper, CONST, pkg\nfrom pkg import other\n");
    let fx = fixture(t.path());
    assert_eq!(
        fx.rows("pkg"),
        s(&[
            "1 symbol pkg._impl:Thing [exported]",
            "1 symbol pkg._impl:helper [exported]",
            "2 variable pkg._impl [exported]",
            "3 module pkg.other [exported]",
        ])
    );
    assert_eq!(
        fx.rows("user"),
        s(&[
            "1 symbol pkg._impl:Thing [reexport]",
            "1 symbol pkg._impl:helper [reexport]",
            "1 variable pkg._impl [reexport]",
            // `import pkg.other` in pkg binds `pkg` itself.
            "1 module pkg [reexport]",
            // A submodule bound by an import of the package.
            "2 module pkg.other",
        ])
    );
    // The first hop is the import row of `pkg` that binds the name.
    let first = fx.ix.file_facts(fx.file("user")).imports.start as usize;
    let via = fx.imp.via(first).expect("via") as usize;
    assert!(fx.ix.file_facts(fx.file("pkg")).imports.range().contains(&via));
    assert_eq!(fx.ix.facts().imports.line[via], 1);
    // Symbols go to the named module in the module graph (what runs at import time).
    assert_eq!(fx.edges("user"), s(&["pkg x4", "pkg.other x1"]));
}

#[test]
fn exported_flag_rules() {
    let t = TempDir::new("exported");
    t.file("pkg/__init__.py", "from .a import x, _y\nimport os\n")
        .file("pkg/a.py", "x = 1\n_y = 2\n")
        .file("pkg/b.py", "from .a import x\nfrom .a import x as x2\nfrom .a import _y as _y\n")
        .file("pkg/c.py", "from .a import x, _y\n__all__ = ['_y']\n");
    let fx = fixture(t.path());
    // Public names of an `__init__` without `__all__`.
    assert_eq!(fx.rows("pkg"), s(&["1 variable pkg.a [exported]", "1 variable pkg.a", "2 stdlib os [exported]"]));
    // A plain module re-exports only through `as` with the same name.
    assert_eq!(fx.rows("pkg.b"), s(&["1 variable pkg.a", "2 variable pkg.a", "3 variable pkg.a [exported]"]));
    // `__all__` decides.
    assert_eq!(fx.rows("pkg.c"), s(&["1 variable pkg.a", "1 variable pkg.a [exported]"]));
    assert!(fx.imp.in_all(fx.file("pkg.c"), fx.sym("_y")));
    assert!(!fx.imp.in_all(fx.file("pkg.c"), fx.sym("x")));
}

#[test]
fn star_imports_and_all() {
    let t = TempDir::new("star");
    t.file("pkg/__init__.py", "from .api import *\n")
        .file("pkg/api.py", "from .core import *\ndef api_f():\n    pass\ndef _private():\n    pass\n")
        .file("pkg/core.py", "__all__ = ['core_f', 'sub']\ndef core_f():\n    pass\ndef hidden():\n    pass\n")
        .file("pkg/core/sub.py", "")
        .file("user.py", "from pkg import api_f, core_f, hidden, _private\nfrom pkg.core import hidden\nfrom pkg import *\n");
    let fx = fixture(t.path());
    assert_eq!(fx.rows("pkg"), s(&["1 module pkg.api [star]"]));
    assert_eq!(
        fx.rows("user"),
        s(&[
            "1 symbol pkg.api:api_f [viastar]",
            "1 symbol pkg.core:core_f [viastar]",
            // Not in `__all__` of core / private in api: not star-exported.
            "1 missing-name in pkg",
            "1 missing-name in pkg",
            // Direct imports ignore `__all__`.
            "2 symbol pkg.core:hidden",
            "3 module pkg [star]",
        ])
    );
    // Names bound through star imports.
    assert_eq!(fx.global("user", "api_f").as_deref(), Some("def pkg.api:api_f"));
    assert_eq!(fx.global("user", "core_f").as_deref(), Some("def pkg.core:core_f"));
    assert_eq!(fx.global("pkg", "core_f").as_deref(), Some("def pkg.core:core_f"));
    assert_eq!(fx.global("pkg", "hidden"), None);
    let core = fx.imp.modules().lookup(&fx.ix, b"pkg.core").expect("pkg.core");
    let g = fx.imp.attribute(&fx.ix, core, fx.sym("core_f")).map(|g| fx.global_str(g));
    assert_eq!(g.as_deref(), Some("def pkg.core:core_f"));
}

#[test]
fn star_and_reexport_cycles_terminate() {
    let t = TempDir::new("cycles");
    t.file("a.py", "from b import *\nfrom b import X\ndef fa():\n    pass\n")
        .file("b.py", "from a import *\nfrom a import X\ndef fb():\n    pass\n")
        .file("c.py", "from a import fb, fa, nope, X\n");
    let fx = fixture(t.path());
    assert_eq!(
        fx.rows("c"),
        s(&["1 symbol b:fb [viastar]", "1 symbol a:fa", "1 missing-name in a", "1 dynamic in a"])
    );
}

/// A star-import search for one name must not hide the same module from the search for
/// another name in the same lookup (here `b` is re-exported as `z.a`, found in `s1`).
#[test]
fn star_visits_are_per_name() {
    let t = TempDir::new("starname");
    t.file("m.py", "from s1 import *\n")
        .file("s1.py", "from z import a as b\ndef a():\n    pass\n")
        .file("z.py", "from s1 import *\n")
        .file("user.py", "from m import b\n");
    let fx = fixture(t.path());
    assert_eq!(fx.rows("user"), s(&["1 symbol s1:a [reexport,viastar]"]));
}

#[test]
fn dynamic_modules() {
    let t = TempDir::new("dynamic");
    t.file("lazy.py", "def __getattr__(name):\n    return name\n")
        .file("starry.py", "from numpy import *\n")
        .file("user.py", "from lazy import thing\nfrom starry import array\n");
    let fx = fixture(t.path());
    assert_eq!(fx.rows("user"), s(&["1 dynamic in lazy", "2 dynamic in starry"]));
}

#[test]
fn module_attribute_aliases() {
    let t = TempDir::new("aliases");
    // `os.path` / `collections.abc` style: a module bound in its parent, registered in
    // `sys.modules`, with no file of that dotted name.
    t.file("myos.py", "import sys\nimport posixish as path\nsys.modules['myos.path'] = path\n")
        .file("posixish.py", "def join():\n    pass\n")
        .file("coll/__init__.py", "import _coll_abc\nabc = _coll_abc\nclass Real:\n    pass\nAlias = Real\nChained = abc.Mapping\nNum = 3\n")
        .file("_coll_abc.py", "class Mapping:\n    pass\n")
        .file(
            "user.py",
            concat!(
                "import myos.path\n",
                "from myos.path import join\n",
                "from myos import path\n",
                "import coll.abc\n",
                "from coll.abc import Mapping\n",
                "from coll import Alias, Chained, Num\n",
                "import myos.nothing\n",
                "from myos import __name__, __doc__\n",
            ),
        );
    let fx = fixture(t.path());
    assert_eq!(
        fx.rows("user"),
        s(&[
            "1 module posixish",
            "2 symbol posixish:join",
            "3 module posixish [reexport]",
            "4 module _coll_abc",
            "5 symbol _coll_abc:Mapping",
            "6 symbol coll:Real",
            "6 symbol _coll_abc:Mapping [reexport]",
            "6 variable coll",
            "7 missing-module in myos",
            "8 variable myos",
            "8 variable myos",
        ])
    );
    // `import myos.path` / `import myos.nothing` bind `myos` either way.
    assert_eq!(fx.global("user", "myos").as_deref(), Some("module myos"));
    // Module-level aliases follow to what they name; other values stay variables.
    assert_eq!(fx.global("coll", "Alias").as_deref(), Some("def coll:Real"));
    assert_eq!(fx.global("coll", "abc").as_deref(), Some("module _coll_abc"));
    assert_eq!(fx.global("coll", "Chained").as_deref(), Some("def _coll_abc:Mapping"));
    assert_eq!(fx.global("coll", "Num").as_deref(), Some("var coll"));
    assert_eq!(fx.edges("user"), s(&["_coll_abc x1", "coll x4", "myos x4", "posixish x2"]));
}

#[test]
fn dynamic_namespaces() {
    let t = TempDir::new("dynns");
    t.file("g.py", "globals().update({'made': 1})\n")
        .file("e.py", "def make():\n    exec('def made(): pass', globals())\nmake()\n")
        .file("plain.py", "def f():\n    pass\n")
        .file("user.py", "from g import made\nfrom e import made as m2\nfrom plain import made as m3\n");
    let fx = fixture(t.path());
    assert_eq!(fx.rows("user"), s(&["1 dynamic in g", "2 dynamic in e", "3 missing-name in plain"]));
}

#[test]
fn context_flags_and_edge_aggregation() {
    let t = TempDir::new("flags");
    t.file("a.py", "def f():\n    pass\n")
        .file("b.py", "def g():\n    pass\n")
        .file("c.py", "")
        .file("d.py", "")
        .file(
            "user.py",
            concat!(
                "from typing import TYPE_CHECKING\n",
                "if TYPE_CHECKING:\n",
                "    from a import f\n",
                "    import b\n",
                "from b import g\n",
                "try:\n",
                "    import c\n",
                "except ImportError:\n",
                "    c = None\n",
                "if f:\n",
                "    import d\n",
                "def h():\n",
                "    import d\n",
            ),
        );
    let fx = fixture(t.path());
    assert_eq!(
        fx.rows("user"),
        s(&[
            "1 stdlib typing",
            "3 symbol a:f [type,cond]",
            "4 module b [type,cond]",
            "5 symbol b:g",
            "7 module c [cond,try]",
            "11 module d [cond]",
            "13 module d [lazy]",
        ])
    );
    // An edge is type-only / conditional only if every row behind it is.
    assert_eq!(fx.edges("user"), s(&["a x1 [type,cond]", "b x2", "c x1 [cond,try]", "d x2"]));
    assert_eq!(fx.ext_edges("user"), s(&["Stdlib typing x1"]));
}

#[test]
fn attribution_of_unresolved_imports() {
    let t = TempDir::new("attr");
    t.file("pkg/__init__.py", "import numpy as np\n")
        .file("ns/inner/x.py", "")
        .file(
            "user.py",
            concat!(
                "import os.path\n",
                "import json, sys\n",
                "import numpy.linalg\n",
                "from requests import get\n",
                "import zzz_unknown\n",
                "import ns.inner.x\n",
                "import ns.other\n",
                "from pkg import np\n",
                "import __future__\n",
            ),
        );
    let mut attr = Attribution::new();
    attr.add_dist_name(b"numpy", DistId(7));
    attr.add_dist_name(b"requests", DistId(3));
    // Stdlib names win over distributions.
    attr.add_dist_name(b"json", DistId(9));
    let fx = fixture_with(t.path(), &attr);
    assert_eq!(
        fx.rows("user"),
        s(&[
            "1 stdlib os",
            "2 stdlib json",
            "2 stdlib sys",
            "3 third-party dist7",
            "4 third-party dist3",
            "5 external zzz_unknown",
            "6 module ns.inner.x",
            // Only a namespace prefix: attributed by the top-level name.
            "7 external ns",
            // A re-exported third-party module keeps its attribution.
            "8 third-party dist7 [reexport]",
            "9 stdlib __future__",
        ])
    );
    assert_eq!(
        fx.ext_edges("user"),
        s(&["Stdlib __future__ x1", "Stdlib json x1", "Stdlib os x1", "Stdlib sys x1", "dist3 x1", "dist7 x1", "Unknown ns x1", "Unknown zzz_unknown x1"])
    );
    assert_eq!(fx.edges("user"), s(&["ns.inner.x x1", "pkg x1"]));
    assert_eq!(fx.global("user", "np").as_deref(), Some("unresolved 8 third-party dist7 [reexport]"));
    assert!(is_builtin_stdlib(b"asyncio") && is_builtin_stdlib(b"_io") && !is_builtin_stdlib(b"numpy"));
}

#[test]
fn stdlib_table_is_sorted_and_unique() {
    for w in attrib::STDLIB_NAMES.windows(2) {
        assert!(w[0].as_bytes() < w[1].as_bytes(), "{} >= {}", w[0], w[1]);
    }
    for name in ["__future__", "__main__", "os", "zoneinfo", "_abc", "tomllib"] {
        assert!(is_builtin_stdlib(name.as_bytes()), "{name}");
    }
    for name in ["numpy", "requests", "", "os.path"] {
        assert!(!is_builtin_stdlib(name.as_bytes()), "{name}");
    }
}

#[test]
fn stdlib_dir_and_roots_attribution() {
    let t = TempDir::new("stdlibdir");
    t.file("lib/python3.99/brand_new_mod.py", "")
        .file("lib/python3.99/newpkg/__init__.py", "")
        .file("lib/python3.99/lib-dynload/_fastthing.cpython-399-darwin.so", "")
        .file("lib/python3.99/site-packages/notstd.py", "");
    let mut a = Attribution::new();
    a.add_stdlib_dir(&t.path().join("lib/python3.99"));
    a.add_dist_name(b"newpkg", DistId(1));
    assert_eq!(a.classify(b"brand_new_mod"), Attributed::Stdlib);
    assert_eq!(a.classify(b"newpkg"), Attributed::Stdlib);
    assert_eq!(a.classify(b"_fastthing"), Attributed::Stdlib);
    assert_eq!(a.classify(b"site-packages"), Attributed::Unknown);
    assert_eq!(a.classify(b"notstd"), Attributed::Unknown);
    assert_eq!(a.classify(b"os"), Attributed::Stdlib);
    // A distribution name added before the stdlib directory still loses to it.
    let mut b = Attribution::new();
    b.add_dist_name(b"brand_new_mod", DistId(2));
    b.add_stdlib_dir(&t.path().join("lib/python3.99"));
    assert_eq!(b.classify(b"brand_new_mod"), Attributed::Stdlib);
}

/// End to end with discovered roots: deps and stdlib not loaded, attributed from the venv's
/// distributions and the base interpreter's stdlib directory.
#[test]
fn attribution_from_discovered_roots() {
    use crate::roots::{RootOptions, discover};
    let t = TempDir::new("roots");
    let root = t.path().display().to_string();
    let site = "proj/.venv/lib/python3.12/site-packages";
    t.file("proj/pyproject.toml", "[project]\nname = \"proj\"\n")
        .file("proj/src/proj/__init__.py", "import requests\nimport yaml\nimport json\nimport brandnew\nfrom . import core\n")
        .file("proj/src/proj/core.py", "")
        .file("proj/.venv/pyvenv.cfg", &format!("home = {root}/python/bin\nversion_info = 3.12.3\n"))
        .file("python/lib/python3.12/json/__init__.py", "")
        .file("python/lib/python3.12/os.py", "")
        .file("python/lib/python3.12/brandnew.py", "")
        .file(&format!("{site}/requests/__init__.py"), "")
        .file(&format!("{site}/requests-2.0.dist-info/METADATA"), "Metadata-Version: 2.1\nName: requests\nVersion: 2.0\n")
        .file(&format!("{site}/requests-2.0.dist-info/RECORD"), "requests/__init__.py,,\n")
        .file(&format!("{site}/PyYAML-6.0.dist-info/METADATA"), "Metadata-Version: 2.1\nName: PyYAML\nVersion: 6.0\n")
        .file(&format!("{site}/PyYAML-6.0.dist-info/top_level.txt"), "yaml\n");
    fs::create_dir_all(t.path().join("python/bin")).expect("mkdir");
    let opts = RootOptions { path_env: Some(Default::default()), ..RootOptions::default() };
    let roots = discover(&t.path().join("proj"), &opts);
    assert!(roots.stdlib.is_some(), "{:?}", roots.diagnostics);
    let w = walk(&[t.path().join("proj/src")], &WalkOptions::default());
    let ix = load(Inputs::from_roots(&roots, &w.files));
    let attr = Attribution::from_roots(&roots);
    let imp = resolve_imports(&ix, &attr);
    let fx = Fx { ix, imp };
    let requests = roots.dists.by_name(b"requests").expect("requests").0;
    let yaml = roots.dists.by_name(b"pyyaml").expect("pyyaml").0;
    assert_eq!(
        fx.rows("proj"),
        vec![
            format!("1 third-party dist{requests} [exported]"),
            format!("2 third-party dist{yaml} [exported]"),
            "3 stdlib json [exported]".to_owned(),
            "4 stdlib brandnew [exported]".to_owned(),
            "5 module proj.core [exported]".to_owned(),
        ]
    );
}

#[test]
fn stubs_and_namespace_packages() {
    let t = TempDir::new("stubs");
    t.file("m.py", "def impl_only():\n    pass\ndef both():\n    pass\n")
        .file("m.pyi", "def both() -> None: ...\ndef stub_only() -> None: ...\n")
        .file("lone.pyi", "def s() -> None: ...\n")
        .file("nsp/a.py", "def fa():\n    pass\n")
        .file("nsp/deep/b.py", "")
        .file("user.py", "from m import impl_only, both, stub_only\nfrom lone import s\nimport nsp.a\nfrom nsp import a, deep\nfrom nsp.deep import b\nimport nsp\n");
    let fx = fixture(t.path());
    assert_eq!(
        fx.rows("user"),
        s(&[
            "1 symbol m:impl_only",
            // The implementation's binding comes first; the stub fills in.
            "1 symbol m:both",
            "1 symbol m#2:stub_only",
            "2 symbol lone:s",
            "3 module nsp.a",
            "4 module nsp.a",
            "4 module nsp.deep",
            "5 module nsp.deep.b",
            "6 module nsp",
        ])
    );
    let mods = fx.imp.modules();
    let m = mods.lookup(&fx.ix, b"m").expect("m");
    assert_eq!(mods.kind(m), ModKind::Module);
    assert_eq!(fx.qid(mods.node(m).expect("node")), "m");
    assert_eq!(fx.qid(mods.stub(m).expect("stub")), "m#2");
    let lone = mods.lookup(&fx.ix, b"lone").expect("lone");
    assert_eq!((mods.kind(lone), mods.stub(lone)), (ModKind::Module, None));
    let nsp = mods.lookup(&fx.ix, b"nsp").expect("nsp");
    assert_eq!(mods.kind(nsp), ModKind::Namespace);
    let mut name = Vec::new();
    mods.dotted(&fx.ix, mods.lookup(&fx.ix, b"nsp.deep.b").expect("b"), &mut name);
    assert_eq!(name, b"nsp.deep.b");
    assert_eq!(mods.lookup(&fx.ix, b"nsp.nothing"), None);
}

/// Files of several groups with explicit statuses (what `Inputs::from_roots` produces).
#[test]
fn groups_shadowing_and_third_party_targets() {
    use crate::roots::FileStatus;
    let t = TempDir::new("groups");
    t.file("app/main.py", "import lib\nfrom lib.core import run\nimport other\nimport json\n")
        .file("app/lib.py", "def shadowed_def():\n    pass\n")
        .file("site/lib/__init__.py", "")
        .file("site/lib/core.py", "def run():\n    pass\n")
        .file("site/other/__init__.py", "")
        .file("std/json/__init__.py", "")
        .file("std/other.py", "");
    let p = |rel: &str| t.path().join(rel).into_os_string().into_encoded_bytes();
    let mut inp = Inputs::new();
    let fp = inp.add_group(b"app", Origin::FirstParty, None);
    let tp = inp.add_group(b"lib-dist", Origin::ThirdParty, Some(DistId(0)));
    let sl = inp.add_group(b"stdlib", Origin::Stdlib, None);
    inp.push(&p("app/main.py"), fp, b"main", FileStatus::Active).expect("push");
    // A first-party module shadowed by the installed package (e.g. a later root).
    inp.push(&p("app/lib.py"), fp, b"lib", FileStatus::Shadowed).expect("push");
    inp.push(&p("site/lib/__init__.py"), tp, b"lib", FileStatus::Active).expect("push");
    inp.push(&p("site/lib/core.py"), tp, b"lib.core", FileStatus::Active).expect("push");
    inp.push(&p("site/other/__init__.py"), tp, b"other", FileStatus::Shadowed).expect("push");
    inp.push(&p("std/json/__init__.py"), sl, b"json", FileStatus::Active).expect("push");
    inp.push(&p("std/other.py"), sl, b"other", FileStatus::Active).expect("push");
    let ix = load(inp);
    let imp = resolve_imports(&ix, &Attribution::new());
    let fx = Fx { ix, imp };
    assert_eq!(
        fx.rows("main"),
        s(&["1 module lib#2", "2 symbol lib.core:run", "3 module other#2", "4 module json"])
    );
    let main = fx.file("main");
    let origins: Vec<Origin> = fx
        .imp
        .module_edges(main)
        .iter()
        .map(|e| fx.ix.file_origin(fx.ix.file(e.dst).expect("file")))
        .collect();
    assert_eq!(origins, [Origin::ThirdParty, Origin::ThirdParty, Origin::Stdlib, Origin::Stdlib]);
    assert!(fx.imp.ext_edges(main).is_empty());
}

/// Dump of every row and edge, independent of `Sym` numbering.
fn dump(fx: &Fx) -> String {
    let mut out = String::new();
    for f in 0..fx.ix.file_count() {
        let _ = writeln!(out, "file {}", String::from_utf8_lossy(fx.ix.str(fx.ix.file_module(f))));
        for row in fx.ix.file_facts(f).imports.range() {
            let via = fx.imp.via(row).map_or(String::new(), |v| format!(" via {v}"));
            let _ = writeln!(out, "  {}{via}", fx.row(row));
        }
        for e in fx.imp.module_edges(f) {
            let _ = writeln!(out, "  -> {} x{} {}", fx.qid(e.dst), e.rows, e.flags);
        }
        for e in fx.imp.ext_edges(f) {
            let key = match e.kind {
                ExtKind::ThirdParty => e.key.to_string(),
                _ => String::from_utf8_lossy(edges::ext_name(&fx.ix, e.key)).into_owned(),
            };
            let _ = writeln!(out, "  => {:?} {key} x{} {}", e.kind, e.rows, e.flags);
        }
    }
    out
}

fn fixture_threads(root: &Path, threads: usize) -> Fx {
    let pool = rayon::ThreadPoolBuilder::new().num_threads(threads).build().expect("pool");
    pool.install(|| fixture(root))
}

fn corpus_root(sub: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/corpus").join(sub)
}

#[test]
fn corpus_sample_python_resolves_fully() {
    let root = corpus_root("sample_python");
    let fx = fixture_threads(&root, 4);
    for row in 0..fx.imp.len() {
        let st = fx.imp.status(row);
        assert!(matches!(st, ImportStatus::Symbol | ImportStatus::Module), "row {row}: {}", fx.row(row));
    }
    assert_eq!(
        fx.rows("app"),
        s(&[
            "1 symbol services.auth:require_auth",
            "2 symbol services.user_service:create_user",
            "3 symbol middleware.pipeline:run_pipeline",
            "4 symbol utils.decorators:log_calls",
            "5 symbol cycles:ping",
        ])
    );
    assert_eq!(
        fx.rows("models"),
        s(&[
            "1 symbol models.base:BaseModel [exported]",
            "2 symbol models.user:User [exported]",
            "3 symbol models.product:Product [exported]",
        ])
    );
    assert_eq!(fx.edges("models.user"), s(&["models.base x1"]));
    assert_eq!(dump(&fixture_threads(&root, 1)), dump(&fx));
}

#[test]
fn corpus_fixtures_cross_module() {
    let fx = fixture(&corpus_root("fixtures/cross_module"));
    assert_eq!(
        fx.rows("a"),
        s(&["1 symbol b:foo", "2 symbol b:bar", "3 module utils", "4 stdlib os"])
    );
    let a = fx.file("a");
    assert_eq!(fx.imp.global(&fx.ix, a, fx.sym("b_bar")).map(|g| fx.global_str(g)).as_deref(), Some("def b:bar"));
    assert_eq!(fx.ext_edges("a"), s(&["Stdlib os x1"]));
}

#[test]
fn whole_corpus_is_deterministic() {
    let root = corpus_root("");
    let many = fixture_threads(&root, 8);
    assert_eq!(dump(&fixture_threads(&root, 1)), dump(&many));
    let st = many.imp.stats();
    assert_eq!(st.rows, many.ix.facts().imports.len() as u64);
    assert_eq!(st.by_status.iter().sum::<u64>(), st.rows);
    assert!(many.imp.heap_bytes() > 0);
}

#[test]
fn empty_index() {
    let t = TempDir::new("empty");
    let fx = fixture(t.path());
    assert!(fx.imp.is_empty());
    assert!(fx.imp.modules().is_empty());
    assert_eq!(fx.imp.all_module_edges().count(), 0);
}

/// Real-world run: the stdlib (`PYMAPPER_STDLIB`, else the default path of docs/design.md)
/// resolves deterministically, and nearly every import of it resolves to stdlib code. Slow in
/// debug builds: `cargo test --release -p pymapper-core stdlib_ -- --ignored`.
#[test]
#[ignore]
fn stdlib_imports_resolve() {
    let root = std::env::var_os("PYMAPPER_STDLIB").map(PathBuf::from).unwrap_or_else(|| {
        PathBuf::from("/Users/bt386671/.local/share/uv/python/cpython-3.13.5-macos-aarch64-none/lib/python3.13")
    });
    if !root.is_dir() {
        eprintln!("skipping: {} not found", root.display());
        return;
    }
    let mut attr = Attribution::new();
    attr.add_stdlib_dir(&root);
    let many = fixture_threads_with(&root, 14, &attr);
    let st = *many.imp.stats();
    // Stdlib-attributed rows are built-in / extension modules (no source); dynamic ones are
    // `globals()` / `__getattr__` modules. The rest would be resolver bugs or real breakage
    // (`idlelib.idle_test` is not shipped, PEP 695 `type` statements are not extracted yet).
    let unexpected = st.by_status[ImportStatus::MissingName as usize]
        + st.by_status[ImportStatus::MissingModule as usize]
        + st.by_status[ImportStatus::BeyondTop as usize]
        + st.by_status[ImportStatus::External as usize];
    eprintln!("stdlib imports: {st:?}");
    // `PYMAPPER_DEBUG_RESOLVE=1` prints the most common unresolved imports;
    // `=missing-name` (any status name) only those of that status.
    if let Some(only) = std::env::var_os("PYMAPPER_DEBUG_RESOLVE") {
        let only = only.to_string_lossy().into_owned();
        let mut hist: std::collections::HashMap<String, u32> = std::collections::HashMap::new();
        for f in 0..many.ix.file_count() {
            for row in many.ix.file_facts(f).imports.range() {
                let st = many.imp.status(row);
                let wanted = if only == "1" {
                    matches!(st, ImportStatus::External | ImportStatus::MissingModule | ImportStatus::MissingName)
                } else {
                    st.as_str() == only
                };
                if wanted {
                    let segs = many.ix.file_view(f).import_module(row);
                    let m: Vec<String> = segs.iter().map(|&s| String::from_utf8_lossy(many.ix.str(s)).into_owned()).collect();
                    let lvl = many.ix.facts().imports.level[row];
                    let name = many.ix.facts().imports.name[row].map_or(String::new(), |n| String::from_utf8_lossy(many.ix.str(n)).into_owned());
                    let from = String::from_utf8_lossy(many.ix.str(many.ix.file_module(f))).into_owned();
                    let key = format!("{} {}{} {name} (in {from})", st.as_str(), ".".repeat(lvl as usize), m.join("."));
                    *hist.entry(key).or_default() += 1;
                }
            }
        }
        let mut v: Vec<_> = hist.into_iter().collect();
        v.sort_by_key(|a| std::cmp::Reverse(a.1));
        for (k, n) in v.iter().take(40) {
            eprintln!("{n:6} {k}");
        }
    }
    assert!(unexpected * 100 <= st.rows * 3, "{unexpected} of {} rows unresolved", st.rows);
    // Spot checks when the root is a stdlib (the variable may point at any tree).
    if many.imp.modules().lookup(&many.ix, b"json.decoder").is_some() {
        let rows: Vec<String> = many.rows("json").into_iter().filter(|r| r.contains("JSONDecoder")).collect();
        assert_eq!(rows.len(), 1);
        assert!(rows[0].ends_with("symbol json.decoder:JSONDecoder [exported]"), "{rows:?}");
        let rows = many.rows("collections");
        assert!(rows.iter().any(|r| r.ends_with("module _collections_abc")), "{rows:?}");
    }
    let one = fixture_threads_with(&root, 1, &attr);
    assert_eq!(dump(&one), dump(&many));
}

/// The stdlib as one root, without the interpreter's site-packages (like `Roots::sources`).
fn fixture_threads_with(root: &Path, threads: usize, attr: &Attribution) -> Fx {
    let pool = rayon::ThreadPoolBuilder::new().num_threads(threads).build().expect("pool");
    pool.install(|| {
        let opts = WalkOptions { excludes: vec!["/site-packages".to_owned()], ..WalkOptions::everything() };
        let w = walk(&[root], &opts);
        let ix = load(Inputs::from_root_dir(root, &w.files, Origin::Stdlib, b"stdlib"));
        let imp = resolve_imports(&ix, attr);
        Fx { ix, imp }
    })
}
