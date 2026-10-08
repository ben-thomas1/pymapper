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

// ---------------------------------------------------------------------------------------------
// Calls
// ---------------------------------------------------------------------------------------------

#[test]
fn call_receivers_and_positions() {
    let src = "\
class C(B):
    def m(self):
        f(1)
        a.b.g()
        self.x.h()
        cls.k()
        super().m()
        super(C, self).n()
        f().g()
        x[0].y()
        ''.join(z)
";
    // Positions are those of the first stored name (after `super()` / the unknown receiver).
    check_lines(
        src,
        "call",
        "call 2:m L3:8 f()
         call 2:m L4:8 a.b.g() dotted
         call 2:m L5:8 self.x.h() self
         call 2:m L6:8 cls.k() cls
         call 2:m L7:8 super()
         call 2:m L7:16 m() super
         call 2:m L8:8 super()
         call 2:m L8:23 n() super
         call 2:m L9:8 f()
         call 2:m L9:12 g() unknown
         call 2:m L10:13 y() unknown
         call 2:m L11:11 join() unknown",
    );
}

#[test]
fn call_scopes() {
    let src = "\
@deco(arg())
def f(x=default()):
    g()
    h = lambda: inner()
    class K(base()):
        attr = make()
        def m(self):
            return [n() for _ in it()]
top()
";
    check_lines(
        src,
        "call",
        "call 0:<module> L1:1 deco()
         call 0:<module> L1:6 arg()
         call 0:<module> L2:8 default()
         call 1:f L3:4 g()
         call 1:f L4:16 inner() [lambda]
         call 1:f L5:12 base()
         call 2:K L6:15 make()
         call 3:m L8:20 n()
         call 3:m L8:33 it()
         call 0:<module> L9:0 top()",
    );
}

#[test]
fn main_guard_and_type_checking_calls() {
    let src = "\
def main(): pass
if __name__ == '__main__':
    main()
    if x:
        other()
after()
def f():
    if __name__ == \"__main__\":
        nested()
if TYPE_CHECKING:
    cast()
";
    // evidence includes the string "__main__".
    assert_eq!(header(src), "file lines=11 evidence=9 [main_guard]");
    check_lines(
        src,
        "call",
        "call 0:<module> L3:4 main() [main_guard]
         call 0:<module> L5:8 other() [main_guard]
         call 0:<module> L6:0 after()
         call 2:f L9:8 nested()
         call 0:<module> L11:4 cast() [type_checking]",
    );
    // A guard only inside a function does not count.
    assert_eq!(header("def f():\n    if __name__ == '__main__':\n        g()\n"), "file lines=3 evidence=3");
}

#[test]
fn long_call_chain_keeps_the_callee() {
    let chain: Vec<String> = (0..300).map(|i| format!("n{i}")).collect();
    let src = format!("{}()\nimport {}\n", chain.join("."), chain.join("."));
    let mut st = FactStore::new();
    let mut spans = SpanStrs::new();
    let ff = extract(src.as_bytes(), &mut st, &mut spans);
    let v = st.file(&ff);
    let c = v.call_chain(ff.calls.start as usize);
    assert_eq!(c.len(), MAX_CHAIN);
    assert_eq!(spans.text(src.as_bytes(), c[0]), b"n45");
    assert_eq!(spans.text(src.as_bytes(), c[MAX_CHAIN - 1]), b"n299");
    assert_eq!(receiver(st.calls.receiver[ff.calls.start as usize]), Receiver::Unknown);
    let m = v.import_module(ff.imports.start as usize);
    assert_eq!(m.len(), MAX_CHAIN);
    assert_eq!(spans.text(src.as_bytes(), m[0]), b"n0");
}

// ---------------------------------------------------------------------------------------------
// Bindings, __all__, decls
// ---------------------------------------------------------------------------------------------

#[test]
fn binding_kinds_and_values() {
    let src = "\
x = Foo(1)
y = m.Bar()
z = a.b
w = [1]
a = b = make()
c, *d = pair()
e: int = 3
f: str
g += 1
self.attr = Thing()
obj.items[0] = 1
for i, j in it:
    pass
with open(p) as fh:
    pass
if (n := 10) > 5:
    pass
try:
    pass
except E as err:
    pass
";
    // Header targets (for / walrus in a condition / except-as) take the context of the
    // statement header, not of the block.
    check_lines(
        src,
        "bind",
        "bind 0:<module> L1 x = Foo()
         bind 0:<module> L2 y = m.Bar()
         bind 0:<module> L3 z = a.b
         bind 0:<module> L4 w = <expr>
         bind 0:<module> L5 a = make()
         bind 0:<module> L5 b = make()
         bind 0:<module> L6 c = <expr> [unpacked]
         bind 0:<module> L6 d = <expr> [unpacked,starred]
         bind 0:<module> L7 e : <expr>
         bind 0:<module> L8 f :
         bind 0:<module> L9 g op= <expr>
         bind 0:<module> L10 self.attr = Thing()
         bind 0:<module> L12 i for [unpacked]
         bind 0:<module> L12 j for [unpacked]
         bind 0:<module> L14 fh as
         bind 0:<module> L16 n := <expr>
         bind 0:<module> L20 err as",
    );
}

#[test]
fn binding_value_chain_reuses_the_call_chain() {
    let src = "x = pkg.mod.Factory()\n";
    let mut st = FactStore::new();
    let mut spans = SpanStrs::new();
    let ff = extract(src.as_bytes(), &mut st, &mut spans);
    assert_eq!(ff.calls.len, 1);
    assert_eq!(ff.bindings.len, 1);
    // The call's three names are stored once; the target `x` is inline.
    assert_eq!(ff.names.len, 3);
    let v = st.file(&ff);
    assert_eq!(v.binding_value(ff.bindings.start as usize), v.call_chain(ff.calls.start as usize));
}

#[test]
fn bindings_in_scopes() {
    let src = "\
class C:
    attr = 1
    def m(self):
        self.x = Y()
        local = 2
";
    check_lines(
        src,
        "bind",
        "bind 1:C L2 attr = <expr>
         bind 2:m L4 self.x = Y()
         bind 2:m L5 local = <expr>",
    );
}

#[test]
fn dunder_all_static() {
    let src = "__all__ = ['a', \"b\"]\n__all__ += ('c',)\nif x:\n    __all__ += [r'd']\n";
    assert_eq!(header(src), "file lines=4 evidence=6 [all_defined]");
    check_lines(src, "all", "all [a, b, c, d]");
    // Reassignment replaces.
    check_lines("__all__ = ['a']\n__all__ = ['b', '''c''']\n", "all", "all [b, c]");
    // Annotated.
    check_lines("__all__: list[str] = ['a']\n", "all", "all [a]");
    // Empty.
    assert_eq!(header("__all__ = []\n"), "file lines=1 evidence=1 [all_defined]");
}

#[test]
fn dunder_all_dynamic() {
    for src in [
        "__all__ = ['a'] + other.__all__\n",
        "__all__ = names\n",
        "__all__ = ['a']\n__all__.extend(['b'])\n",
        "__all__ = ['a']\n__all__ -= ['a']\n",
        "__all__ = [f'{x}']\n",
        "for __all__ in x: pass\n",
    ] {
        let h = header(src);
        assert!(h.contains("all_dynamic"), "{src:?}: {h}");
    }
    // `__all__` in a function or class is not the module's.
    assert_eq!(header("def f():\n    __all__ = ['x']\n"), "file lines=2 evidence=2");
    assert_eq!(lines("class C:\n    __all__ = ['x']\n", "all"), "");
    // Mutation without assignment: dynamic but not defined.
    assert_eq!(header("__all__.append('x')\n"), "file lines=1 evidence=3 [all_dynamic]");
}

#[test]
fn global_and_nonlocal() {
    let src = "\
def f():
    global a, b
    def g():
        nonlocal c
";
    check_lines(
        src,
        "decl",
        "decl 1:f global a
         decl 1:f global b
         decl 2:g nonlocal c",
    );
}

// ---------------------------------------------------------------------------------------------
// Evidence, diagnostics, suppressions
// ---------------------------------------------------------------------------------------------

fn with_view<R>(src: &str, f: impl FnOnce(FileView<'_>) -> R) -> R {
    let mut st = FactStore::new();
    let mut spans = SpanStrs::new();
    let ff = extract(src.as_bytes(), &mut st, &mut spans);
    f(st.file(&ff))
}

#[test]
fn evidence_covers_names_attributes_and_identifier_strings() {
    let src = "\
handler_ref = registry
obj.method_attr
getattr(x, 'string_name')
load('pkg.mod.dotted_fn')
s = 'not an identifier'
t = '1digit'
u = f'fstr_name'
v = b'bytes_name'
w = '''triple_name'''
def defined_only(param_only): pass
";
    with_view(src, |v| {
        for yes in ["handler_ref", "registry", "obj", "method_attr", "getattr", "string_name", "pkg", "mod", "dotted_fn", "bytes_name", "triple_name"] {
            assert!(v.has_evidence(yes.as_bytes()), "{yes}");
        }
        // Def and param names are definitions, not references; f-strings are not plain strings.
        for no in ["defined_only", "param_only", "not an identifier", "1digit", "fstr_name", "pkg.mod.dotted_fn"] {
            assert!(!v.has_evidence(no.as_bytes()), "{no}");
        }
        let ev = v.evidence();
        assert!(ev.windows(2).all(|w| w[0] < w[1]), "sorted and unique");
    });
}

#[test]
fn evidence_dedups_many_repeats_and_collisions() {
    // Far more distinct names than filter slots, each repeated: the column stays exact.
    let mut src = String::new();
    for round in 0..3 {
        for i in 0..5000 {
            src.push_str(&format!("n{i}.a{}  # round {round}\n", i % 7));
        }
    }
    with_view(&src, |v| {
        assert_eq!(v.evidence().len(), 5000 + 7);
        assert!(v.evidence().windows(2).all(|w| w[0] < w[1]));
        assert!(v.has_evidence(b"n4999") && v.has_evidence(b"a6"));
    });
}

#[test]
fn diagnostics_are_recorded() {
    let src = "def f(:\n    pass\nprint 'hello'\nx = (1,\n";
    let d = lines(src, "diag");
    assert!(d.contains("diag L3:0 Python2"), "{d}");
    assert!(d.contains("UnclosedBracket"), "{d}");
    assert!(!d.is_empty());
    assert_eq!(lines("x = 1\n", "diag"), "");
}

#[test]
fn suppressions_are_recorded_with_lines_and_codes() {
    let src = "\
import os  # pymapper: ignore[PM101, PM202]
x = 1
def f():  # noqa: E501  # pymapper: ignore
    pass  # pymapper:ignore[PM3]
";
    check_lines(
        src,
        "suppress",
        "suppress L1 [PM101, PM202]
         suppress L3 all
         suppress L4 [PM3]",
    );
}

// ---------------------------------------------------------------------------------------------
// Store: several files, merging, interning
// ---------------------------------------------------------------------------------------------

const FILE_A: &str = "\
import os
from .x import y as z
__all__ = ['A']
class A(Base):
    @property
    def p(self):
        return self.q.r(os.path.join(a, b))
if __name__ == '__main__':
    A().p  # pymapper: ignore[PM1]
";
const FILE_B: &str = "def g(x, *a, **k):\n    global G\n    G = h(x)\n    return lambda: k.get('key')\nprint 'py2'\n";

#[test]
fn files_in_one_store_render_like_separate_stores() {
    let mut st = FactStore::new();
    let mut spans_a = SpanStrs::new();
    let mut spans_b = SpanStrs::new();
    let fa = extract(FILE_A.as_bytes(), &mut st, &mut spans_a);
    let fb = extract(FILE_B.as_bytes(), &mut st, &mut spans_b);
    let ra = render(st.file(&fa), |s| spans_a.text(FILE_A.as_bytes(), s));
    let rb = render(st.file(&fb), |s| spans_b.text(FILE_B.as_bytes(), s));
    assert_eq!(ra, facts(FILE_A));
    assert_eq!(rb, facts(FILE_B));
    // Headers tile the tables.
    assert_eq!(fa.defs.start + fa.defs.len, fb.defs.start);
    assert_eq!(fa.names.start + fa.names.len, fb.names.start);
    assert_eq!(fb.evidence.start + fb.evidence.len, st.evidence.len() as u32);
}

#[test]
fn append_and_shift_preserve_every_file() {
    // Worker stores, interning into per-worker interners (the loader's shape).
    let mut w1 = (FactStore::new(), Interner::new());
    let mut w2 = (FactStore::new(), Interner::new());
    let f1 = extract(FILE_A.as_bytes(), &mut w1.0, &mut w1.1);
    let mut f2 = extract(FILE_B.as_bytes(), &mut w2.0, &mut w2.1);
    let mut f3 = extract(FILE_A.as_bytes(), &mut w2.0, &mut w2.1);
    let r1 = render(w1.0.file(&f1), |s| w1.1.resolve(s));
    let r2 = render(w2.0.file(&f2), |s| w2.1.resolve(s));
    let r3 = render(w2.0.file(&f3), |s| w2.1.resolve(s));
    assert_eq!(r1, facts(FILE_A), "interner sink renders like the span sink");
    assert_eq!(r3, r1);

    // Merge interners, remap worker 2's ids, then concatenate the stores.
    let (global, remaps) = Interner::merge_all(vec![w1.1, w2.1]);
    for ff in [&f2, &f3] {
        w2.0.map_syms(ff, |s| remaps[1].get(s));
    }
    w1.0.map_syms(&f1, |s| remaps[0].get(s));
    let mut merged = w1.0;
    let bases = merged.append(&mut w2.0);
    assert!(w2.0.defs.is_empty() && w2.0.names.is_empty());
    f2.shift(&bases);
    f3.shift(&bases);
    assert_eq!(render(merged.file(&f1), |s| global.resolve(s)), r1);
    assert_eq!(render(merged.file(&f2), |s| global.resolve(s)), r2);
    assert_eq!(render(merged.file(&f3), |s| global.resolve(s)), r3);
}

#[test]
fn spans_can_be_interned_after_extraction() {
    let mut st = FactStore::new();
    let mut spans = SpanStrs::new();
    let ff = extract(FILE_A.as_bytes(), &mut st, &mut spans);
    let mut interner = Interner::new();
    st.map_syms(&ff, |s| interner.intern(spans.text(FILE_A.as_bytes(), s)));
    assert_eq!(render(st.file(&ff), |s| interner.resolve(s)), facts(FILE_A));
}

#[test]
fn store_reuse_after_clear_is_identical() {
    let mut st = FactStore::new();
    let mut spans = SpanStrs::new();
    let a = extract(FILE_B.as_bytes(), &mut st, &mut spans);
    let first = render(st.file(&a), |s| spans.text(FILE_B.as_bytes(), s));
    st.clear();
    let b = extract(FILE_B.as_bytes(), &mut st, &mut spans);
    assert_eq!(a, b);
    assert_eq!(render(st.file(&b), |s| spans.text(FILE_B.as_bytes(), s)), first);
    assert!(st.row_bytes() > 0 && st.heap_bytes() >= st.row_bytes());
}

#[test]
fn file_b_exact() {
    check(
        FILE_B,
        // evidence: G, h, x, k, get, 'key' (the Python 2 line is skipped after its diagnostic).
        "file lines=5 evidence=6
         module 0:<module> L1-5 b0-89
         def 1:g in=0 L1-4 b0-76 @1:4 (x, *a, **k)
         call 1:g L3:8 h()
         call 1:g L4:19 k.get() dotted [lambda]
         bind 1:g L3 G = h()
         decl 1:g global G
         diag L5:0 Python2",
    );
}

// ---------------------------------------------------------------------------------------------
// Robustness
// ---------------------------------------------------------------------------------------------

/// Deterministic xorshift for generated inputs.
fn xorshift(state: &mut u64) -> u64 {
    let mut x = *state;
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    *state = x;
    x
}

#[test]
fn arbitrary_bytes_never_panic_and_keep_invariants() {
    const PIECES: &[&[u8]] = &[
        b"def ", b"class ", b"if ", b"else:", b"try:", b"except ImportError:", b"import ", b"from ", b" . ",
        b"(", b")", b"[", b"]", b"{", b"}", b":", b",", b"=", b"+=", b"*", b"**", b"@", b"\n", b"\n    ",
        b"\t", b"x", b"self", b"__all__", b"'s'", b"\"\"\"", b"f'{", b"lambda", b"global ", b"\\\n", b"\xff",
        b"\r", b"# pymapper: ignore[A]", b"TYPE_CHECKING", b"__name__ == '__main__'", b"super().", b"\0",
    ];
    let mut rng = 0x9e37_79b9_7f4a_7c15u64;
    let mut st = FactStore::new();
    let mut spans = SpanStrs::new();
    let mut buf = Vec::new();
    for _ in 0..3000 {
        buf.clear();
        let n = xorshift(&mut rng) % 60;
        for _ in 0..n {
            buf.extend_from_slice(PIECES[(xorshift(&mut rng) % PIECES.len() as u64) as usize]);
        }
        let ff = extract(&buf, &mut st, &mut spans);
        let v = st.file(&ff);
        assert_eq!(st.defs.kind[ff.defs.start as usize], DefKind::Module as u8);
        for d in 1..ff.defs.len {
            let row = v.def_row(d);
            assert!(st.defs.parent[row] < d, "parents precede children");
            assert!(st.defs.start[row] <= st.defs.end[row] || st.defs.end[row] == st.defs.start[row]);
            assert!(st.defs.start_line[row] <= st.defs.end_line[row]);
            let _ = (v.decorators(d), v.bases(d), v.params(d));
        }
        for i in ff.calls.range() {
            assert!(st.calls.scope[i] < ff.defs.len);
            let _ = v.call_chain(i);
        }
        for i in ff.bindings.range() {
            let _ = (v.binding_target(i), v.binding_value(i));
        }
        for i in ff.imports.range() {
            let _ = v.import_module(i);
        }
        let _ = render(v, |s| spans.text(&buf, s));
    }
}

#[test]
fn corpus_files_extract_without_diagnostics() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/corpus");
    let walk = crate::walk::walk(&[root], &crate::walk::WalkOptions::everything());
    assert!(walk.files.len() >= 28);
    let mut st = FactStore::new();
    let mut spans = SpanStrs::new();
    for p in walk.files.iter() {
        let src = std::fs::read(p).unwrap_or_default();
        let ff = extract(&src, &mut st, &mut spans);
        assert_eq!(ff.diags.len, 0, "{}", p.display());
    }
}
