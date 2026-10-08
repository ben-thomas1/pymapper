//! FileFacts tests: facts are rendered with [`render`] and compared as text.

use super::*;
use crate::intern::Interner;

/// Extracts `src` and renders its facts (span strings).
fn facts(src: &str) -> String {
    facts_bytes(src.as_bytes())
}

fn facts_bytes(src: &[u8]) -> String {
    let mut st = FactStore::new();
    let mut spans = SpanStrs::new();
    let ff = extract(src, &mut st, &mut spans);
    render(st.file(&ff), |s| spans.text(src, s))
}

/// Asserts the rendered facts equal `expected` (leading indentation and blank lines ignored).
#[track_caller]
fn check(src: &str, expected: &str) {
    let got = facts(src);
    let want: String = expected.lines().map(str::trim).filter(|l| !l.is_empty()).map(|l| format!("{l}\n")).collect();
    assert_eq!(got, want, "\n--- got ---\n{got}--- want ---\n{want}");
}

/// Only the lines of the rendering that start with `prefix`, joined by newlines.
fn lines(src: &str, prefix: &str) -> String {
    facts(src).lines().filter(|l| l.starts_with(prefix)).collect::<Vec<_>>().join("\n")
}

#[track_caller]
fn check_lines(src: &str, prefix: &str, expected: &str) {
    let got = lines(src, prefix);
    let want = expected.lines().map(str::trim).filter(|l| !l.is_empty()).collect::<Vec<_>>().join("\n");
    assert_eq!(got, want, "\n--- got ---\n{got}\n--- want ---\n{want}");
}

/// The `file` header line.
fn header(src: &str) -> String {
    facts(src).lines().next().unwrap_or_default().to_owned()
}

// ---------------------------------------------------------------------------------------------
// Defs
// ---------------------------------------------------------------------------------------------

#[test]
fn empty_file_has_only_the_module_scope() {
    check("", "file lines=0 evidence=0\nmodule 0:<module> L1-1 b0-0");
    check("\n\n", "file lines=2 evidence=0\nmodule 0:<module> L1-2 b0-2");
    check("# just a comment", "file lines=1 evidence=0\nmodule 0:<module> L1-1 b0-16");
}

#[test]
fn line_count_variants() {
    assert_eq!(header("x = 1"), "file lines=1 evidence=1");
    assert_eq!(header("x = 1\n"), "file lines=1 evidence=1");
    assert_eq!(header("x = 1\n\n"), "file lines=2 evidence=1");
    assert_eq!(header("x = 1\r\ny = 2\r\n"), "file lines=2 evidence=2");
    assert_eq!(header("x = 1\ry = 2\rz"), "file lines=3 evidence=3");
    assert_eq!(header("\u{feff}x = 1\n"), "file lines=1 evidence=1");
    assert_eq!(header("s = '''a\nb\nc'''"), "file lines=3 evidence=1");
}

#[test]
fn defs_nested_with_spans_and_parents() {
    check(
        "def f(a, b=1, *args, c, d: int = 2, **kw) -> int:\n    def g():\n        pass\n    return g\n\nclass C:\n    x = 1\n    def m(self, /, y):\n        pass\n",
        // evidence: `int`, `g`, `x` (param names are not evidence).
        "file lines=9 evidence=3
         module 0:<module> L1-9 b0-145
         def 1:f in=0 L1-4 b0-88 @1:4 (a, b=, *args, kw:c, kw:d:=, **kw) [returns]
         def 2:g in=1 L2-3 b54-75 @2:8 () [nested]
         class 3:C in=0 L6-9 b90-144 @6:6
         def 4:m in=3 L8-9 b113-144 @8:8 (/self, y) [method]
         bind 3:C L7 x = <expr>",
    );
}

#[test]
fn def_flags_from_names_and_decorators() {
    let src = "\
class A:
    @property
    def p(self): pass
    @p.setter
    def p(self, v): pass
    @functools.cached_property
    def c(self): pass
    @staticmethod
    def s(): pass
    @classmethod
    def k(cls): pass
    @typing.overload
    def o(self): pass
    @abc.abstractmethod
    def a(self): pass
    @abstractproperty
    def ap(self): pass
    def _priv(self): pass
    def __init__(self): pass
    def __(self): pass
    @app.route('/x', methods=['GET'])
    @login_required
    async def view(self): pass
    @decorators[0]
    def weird(self): pass
";
    // evidence: 16 names/attributes plus the string "GET" ("/x" is not identifier-like).
    check(
        src,
        "file lines=25 evidence=17
         module 0:<module> L1-25 b0-557
         class 1:A in=0 L1-25 b0-556 @1:6
         def 2:p in=1 L2-3 b13-44 @3:8 decs=[property] (self) [property,method]
         def 3:p in=1 L4-5 b49-83 @5:8 decs=[p.setter] (self, v) [property,method]
         def 4:c in=1 L6-7 b88-136 @7:8 decs=[functools.cached_property] (self) [property,method]
         def 5:s in=1 L8-9 b141-172 @9:8 decs=[staticmethod] () [staticmethod,method]
         def 6:k in=1 L10-11 b177-210 @11:8 decs=[classmethod] (cls) [classmethod,method]
         def 7:o in=1 L12-13 b215-253 @13:8 decs=[typing.overload] (self) [overload,method]
         def 8:a in=1 L14-15 b258-299 @15:8 decs=[abc.abstractmethod] (self) [abstract,method]
         def 9:ap in=1 L16-17 b304-344 @17:8 decs=[abstractproperty] (self) [property,abstract,method]
         def 10:_priv in=1 L18-18 b349-370 @18:8 (self) [private,method]
         def 11:__init__ in=1 L19-19 b375-399 @19:8 (self) [dunder,method]
         def 12:__ in=1 L20-20 b404-422 @20:8 (self) [private,method]
         def 13:view in=1 L21-23 b427-511 @23:14 decs=[app.route(), login_required] (self) [async,method,other_decorator]
         def 14:weird in=1 L24-25 b516-556 @25:8 decs=[<expr>] (self) [method,other_decorator]
         call 1:A L21:5 app.route() dotted",
    );
}

#[test]
fn decorators_and_bases_are_recorded_per_def() {
    // `**kw` is reported as starred like `*extra` (the parser does not distinguish them).
    check(
        "@a.b(1)\n@c\nclass K(Base, mod.Other[T], *extra, metaclass=Meta, **kw):\n    @d\n    def f(self): pass\n",
        "file lines=5 evidence=11
         module 0:<module> L1-5 b0-99
         class 1:K in=0 L1-5 b0-98 @3:6 decs=[a.b(), c] bases=[Base, mod.Other[], *extra, metaclass=Meta, *kw] [other_decorator]
         def 2:f in=1 L4-5 b74-98 @5:8 decs=[d] (self) [method,other_decorator]
         call 0:<module> L1:1 a.b() dotted",
    );
}

#[test]
fn def_context_flags() {
    let src = "\
from typing import TYPE_CHECKING
if TYPE_CHECKING:
    def tc(): pass
try:
    def t(): pass
except ImportError:
    pass
with x:
    def w(): pass
";
    check_lines(
        src,
        "def",
        "def 1:tc in=0 L3-3 b55-69 @3:8 () [type_checking,conditional]
         def 2:t in=0 L5-5 b79-92 @5:8 () [conditional]
         def 3:w in=0 L9-9 b134-147 @9:8 ()",
    );
}

// ---------------------------------------------------------------------------------------------
// Imports
// ---------------------------------------------------------------------------------------------

#[test]
fn import_forms() {
    check_lines(
        "import os\nimport os.path as osp, sys\nfrom . import a\nfrom ..pkg.mod import b as c, d\nfrom .x import *\nfrom m import (e,\n    f)\n",
        "import",
        "import 0:<module> L1:7 import os
         import 0:<module> L2:7 import os.path as osp
         import 0:<module> L2:23 import sys
         import 0:<module> L3:14 from . import a
         import 0:<module> L4:22 from ..pkg.mod import b as c
         import 0:<module> L4:30 from ..pkg.mod import d
         import 0:<module> L5:15 from .x import *
         import 0:<module> L6:15 from m import e
         import 0:<module> L7:4 from m import f",
    );
}

#[test]
fn import_module_chain_is_stored_once_per_statement() {
    let src = "from a.b import c, d, e\n";
    let mut st = FactStore::new();
    let mut spans = SpanStrs::new();
    let ff = extract(src.as_bytes(), &mut st, &mut spans);
    assert_eq!(ff.imports.len, 3);
    // `a.b` once in `names` (2 entries); names `c`, `d`, `e` are columns.
    assert_eq!(ff.names.len, 2);
}

#[test]
fn import_scope_and_conditions() {
    let src = "\
import typing
if typing.TYPE_CHECKING:
    import a
elif x:
    import b
else:
    import c
if not TYPE_CHECKING:
    import d
else:
    import e
try:
    import f
    if y:
        import g
except (ValueError, ImportError):
    import h
try:
    import i
except ModuleNotFoundError as exc:
    pass
try:
    import j
except ValueError:
    import k
for _ in y:
    import l
def fn():
    import m
    if TYPE_CHECKING:
        from n import o
";
    check_lines(
        src,
        "import",
        "import 0:<module> L1:7 import typing
         import 0:<module> L3:11 import a [type_checking,conditional]
         import 0:<module> L5:11 import b [conditional]
         import 0:<module> L7:11 import c [conditional]
         import 0:<module> L9:11 import d [conditional]
         import 0:<module> L11:11 import e [type_checking,conditional]
         import 0:<module> L13:11 import f [try_import_error,conditional]
         import 0:<module> L15:15 import g [try_import_error,conditional]
         import 0:<module> L17:11 import h [try_import_error,conditional]
         import 0:<module> L19:11 import i [try_import_error,conditional]
         import 0:<module> L23:11 import j [conditional]
         import 0:<module> L25:11 import k [conditional]
         import 0:<module> L27:11 import l [conditional]
         import 1:fn L29:11 import m
         import 1:fn L31:22 from n import o [type_checking,conditional]",
    );
}

#[test]
fn try_import_error_only_applies_to_its_own_try() {
    let src = "\
try:
    import a
finally:
    pass
try:
    pass
except ImportError:
    pass
";
    check_lines(src, "import", "import 0:<module> L2:11 import a [conditional]");
    // Nested try inside an ImportError handler: the inner try is not flagged by the outer one.
    let src = "\
try:
    import a
except ImportError:
    try:
        import b
    except KeyError:
        import c
";
    check_lines(
        src,
        "import",
        "import 0:<module> L2:11 import a [try_import_error,conditional]
         import 0:<module> L5:15 import b [try_import_error,conditional]
         import 0:<module> L7:15 import c [try_import_error,conditional]",
    );
}
