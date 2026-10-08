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

#[test]
fn dynamic_modules() {
    let t = TempDir::new("dynamic");
    t.file("lazy.py", "def __getattr__(name):\n    return name\n")
        .file("starry.py", "from numpy import *\n")
        .file("user.py", "from lazy import thing\nfrom starry import array\n");
    let fx = fixture(t.path());
    assert_eq!(fx.rows("user"), s(&["1 dynamic in lazy", "2 dynamic in starry"]));
}
