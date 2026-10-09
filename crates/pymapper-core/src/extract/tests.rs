//! Parser tests: a recording visitor renders events as compact text lines.

use super::events::*;
use super::parse;
use crate::lex::Token;

/// Records events as text, one line per event.
struct Rec<'s> {
    src: &'s [u8],
    out: Vec<String>,
    tokens: usize,
}

impl Rec<'_> {
    fn t(&self, t: Token) -> String {
        String::from_utf8_lossy(t.text(self.src)).into_owned()
    }
    fn chain(&self, ts: &[Token]) -> String {
        ts.iter().map(|&t| self.t(t)).collect::<Vec<_>>().join(".")
    }
}

impl Visitor for Rec<'_> {
    fn token(&mut self, _tok: Token) {
        self.tokens += 1;
    }
    fn decorator(&mut self, d: &Decorator<'_>) {
        let c = self.chain(d.names);
        self.out.push(format!("dec {c}{}", if d.called { "()" } else { "" }));
    }
    fn base(&mut self, b: &Base<'_>) {
        let kw = b.keyword.map(|k| format!("{}=", self.t(k))).unwrap_or_default();
        let c = self.chain(b.names);
        let star = if b.starred { "*" } else { "" };
        let sub = if b.subscripted { "[]" } else { "" };
        self.out.push(format!("base {star}{kw}{c}{sub}"));
    }
    fn def_enter(&mut self, d: &Def<'_>) {
        let kind = if d.kind == DefKind::Class { "class" } else { "def" };
        let asy = if d.is_async { "async " } else { "" };
        let mut s = format!("{asy}{kind} {} @{}:{} L{} d{}", self.t(d.name), d.name_pos.line, d.name_pos.col, d.start_line, d.depth);
        if d.decorators > 0 {
            s += &format!(" decs={}", d.decorators);
        }
        if d.bases > 0 {
            s += &format!(" bases={}", d.bases);
        }
        if !d.type_params.is_empty() {
            s += &format!(" [{}]", d.type_params.iter().map(|&t| self.t(t)).collect::<Vec<_>>().join(","));
        }
        if !d.params.is_empty() {
            let ps: Vec<String> = d
                .params
                .iter()
                .map(|p| {
                    let k = match p.kind {
                        ParamKind::PositionalOnly => "/",
                        ParamKind::Normal => "",
                        ParamKind::VarPositional => "*",
                        ParamKind::KeywordOnly => "kw:",
                        ParamKind::VarKeyword => "**",
                    };
                    let a = if p.annotated { ":" } else { "" };
                    let d = if p.has_default { "=" } else { "" };
                    format!("{k}{}{a}{d}", self.t(p.name))
                })
                .collect();
            s += &format!(" ({})", ps.join(","));
        }
        if d.has_return_annotation {
            s += " ->";
        }
        self.out.push(s);
    }
    fn def_exit(&mut self, kind: DefKind, end: End) {
        let kind = if kind == DefKind::Class { "class" } else { "def" };
        self.out.push(format!("end {kind} L{}", end.line));
    }
    fn import(&mut self, i: &Import<'_>) {
        let mut s = String::from(if i.from { "from " } else { "import " });
        s += &".".repeat(i.level as usize);
        s += &self.chain(i.module);
        if let Some(n) = i.name {
            s += &format!(" : {}", self.t(n));
        }
        if i.star {
            s += " : *";
        }
        if let Some(a) = i.alias {
            s += &format!(" as {}", self.t(a));
        }
        s += &format!(" L{}", i.pos.line);
        self.out.push(s);
    }
    fn decl(&mut self, kind: DeclKind, names: &[Token]) {
        let c = names.iter().map(|&t| self.t(t)).collect::<Vec<_>>().join(",");
        self.out.push(format!("{kind:?} {c}"));
    }
    fn block_enter(&mut self, b: &Block<'_>) {
        let mut s = format!("{:?}", b.kind);
        if b.is_async {
            s = format!("async {s}");
        }
        if b.cond != IfCond::Other {
            s += &format!(" {:?}", b.cond);
        }
        if b.star {
            s += "*";
        }
        if !b.except_names.is_empty() {
            s += &format!(" ({})", b.except_names.iter().map(|&t| self.t(t)).collect::<Vec<_>>().join(","));
        }
        if b.irrefutable {
            s += " irrefutable";
        }
        if b.guarded {
            s += " guarded";
        }
        s += &format!(" @{}:{} {{", b.pos.line, b.pos.col);
        self.out.push(s);
    }
    fn block_exit(&mut self, kind: BlockKind, end: End, last_case: bool) {
        let last = if last_case { " last" } else { "" };
        self.out.push(format!("}} {kind:?} L{}{last}", end.line));
    }
    fn call(&mut self, c: &Call<'_>) {
        let c2 = self.chain(c.names);
        self.out.push(format!("call {c2} {:?} {}:{}", c.receiver, c.pos.line, c.pos.col));
    }
    fn keyword_arg(&mut self, name: Token) {
        let n = self.t(name);
        self.out.push(format!("kw {n}"));
    }
    fn assign(&mut self, a: &Assign<'_>) {
        let tgt = self.chain(a.target);
        let mut s = format!("assign {:?} {}{tgt}", a.kind, if a.starred { "*" } else { "" });
        if a.unpacked {
            s += " unpacked";
        }
        let op = self.t(a.op);
        s += &format!(" {op} ");
        s += &match a.value {
            Value::None => "none".to_string(),
            Value::Call { names } => format!("call {}", self.chain(names)),
            Value::Name(n) => format!("name {}", self.chain(n)),
            Value::Strings(st) => format!("strs [{}]", st.iter().map(|&t| self.t(t)).collect::<Vec<_>>().join(",")),
            Value::Other => "other".to_string(),
        };
        s += &format!(" L{}", a.pos.line);
        self.out.push(s);
    }
    fn branch(&mut self, kind: BranchKind, _tok: Token) {
        self.out.push(format!("branch {kind:?}"));
    }
    fn bool_op(&mut self, op: BoolOp, _tok: Token) {
        self.out.push(format!("bool {op:?}"));
    }
    fn lambda_enter(&mut self, _tok: Token) {
        self.out.push("lambda {".to_string());
    }
    fn lambda_exit(&mut self, end: u32) {
        self.out.push(format!("}} lambda {end}"));
    }
    fn type_alias(&mut self, name: Token, tps: &[Token], pos: Pos) {
        let n = self.t(name);
        self.out.push(format!("type {n} [{}] L{}", tps.iter().map(|&t| self.t(t)).collect::<Vec<_>>().join(","), pos.line));
    }
    fn diagnostic(&mut self, d: &Diagnostic) {
        let t = self.t(d.token);
        self.out.push(format!("diag {:?} {}:{} {t}", d.kind, d.pos.line, d.pos.col));
    }
}

fn run(src: &str) -> Rec<'_> {
    parse(src.as_bytes(), Rec { src: src.as_bytes(), out: Vec::new(), tokens: 0 })
}

fn events(src: &str) -> Vec<String> {
    run(src).out
}

#[test]
fn smoke_consumes_every_token() {
    let r = run("x = 1\nif x:\n    y = (1,\n 2)\n");
    assert_eq!(r.tokens, 13);
    assert!(!r.out.iter().any(|e| e.starts_with("diag")), "{:?}", r.out);
}

#[test]
fn noop_visitor_and_empty_inputs() {
    for src in ["", "\n", "   \n\n", "\u{feff}", "#c\n", "x"] {
        parse(src.as_bytes(), NoopVisitor);
    }
}

/// Definition-structure events only.
fn defs(src: &str) -> String {
    const KEEP: [&str; 12] = ["def ", "end ", "class ", "async def", "dec ", "base ", "Global", "Nonlocal", "import ", "from ", "diag", "If @"];
    events(src).into_iter().filter(|e| KEEP.iter().any(|k| e.starts_with(k)) || e.starts_with("} If") || e.starts_with("For @") || e.starts_with("} For")).collect::<Vec<_>>().join("\n")
}

#[test]
fn defs_and_nesting() {
    let src = "def f(a, b=1, *args, c, d: int = 2, **kw) -> int:\n    def g():\n        pass\n    return 1\nclass C(Base, m.Mixin, metaclass=M):\n    x = 1\n    async def h(self, /, x): ...\n";
    assert_eq!(
        defs(src),
        "def f @1:4 L1 d0 (a,b=,*args,kw:c,kw:d:=,**kw) ->\n\
         def g @2:8 L2 d1\n\
         end def L3\n\
         end def L4\n\
         base Base\nbase m.Mixin\nbase metaclass=M\n\
         class C @5:6 L5 d0 bases=3\n\
         async def h @7:14 L7 d1 (/self,x)\n\
         end def L7\n\
         end class L7"
    );
}

#[test]
fn decorators_and_type_params() {
    let src = "@property\n@app.route('/x', methods=['GET'])\n@(lambda f: f)\ndef f[T: int, *Ts, **P = int](x: T) -> T: return x\n\n@dataclass\nclass P[T](Generic[T]): pass\n";
    assert_eq!(
        defs(src),
        "dec property\ndec app.route()\ndec \n\
         def f @4:4 L1 d0 decs=3 [T,Ts,P] (x:) ->\n\
         end def L4\n\
         dec dataclass\nbase Generic[]\n\
         class P @7:6 L6 d0 decs=1 bases=1 [T]\n\
         end class L7"
    );
}

#[test]
fn star_params_and_bare_star() {
    assert_eq!(defs("def f(a, /, b, *, c=1):\n  pass\n"), "def f @1:4 L1 d0 (/a,b,kw:c=)\nend def L2");
    assert_eq!(defs("lambda_ = 1\ndef f(\n  a,\n  b,\n):\n  pass\n"), "def f @2:4 L2 d0 (a,b)\nend def L6");
}

#[test]
fn imports() {
    let src = "import os, a.b.c as d\nfrom . import x\nfrom ..pkg.mod import (y as z,\n   w,)\nfrom ... import *\nfrom m import *\n";
    assert_eq!(
        defs(src),
        "import os L1\nimport a.b.c as d L1\nfrom . : x L2\nfrom ..pkg.mod : y as z L3\nfrom ..pkg.mod : w L4\nfrom ... : * L5\nfrom m : * L6"
    );
}

#[test]
fn decls_and_simple_statements() {
    let src = "def f():\n    global a, b\n    nonlocal c\n    del x; return; pass\n    raise E from e\n    assert x, 'm'\n    break\n    continue\n";
    assert_eq!(defs(src), "def f @1:4 L1 d0\nGlobal a,b\nNonlocal c\nend def L8");
}

#[test]
fn async_def_and_single_line_bodies() {
    let src = "async def f(): await x\nclass E: pass\ndef g(): x = 1; y = 2\n";
    assert_eq!(
        defs(src),
        "async def f @1:10 L1 d0\nend def L1\nclass E @2:6 L2 d0\nend class L2\ndef g @3:4 L3 d0\nend def L3"
    );
}

#[test]
fn def_end_line_with_trailing_comments_and_blank_lines() {
    let src = "def f():\n    x = '''a\nb'''\n\n    # c\n\nz = 1\n";
    assert_eq!(defs(src), "def f @1:4 L1 d0\nend def L3");
}

/// Only block events, compactly: `If{ ... }` on one line.
fn blocks(src: &str) -> String {
    events(src)
        .into_iter()
        .filter_map(|e| {
            if let Some(rest) = e.strip_prefix("} ") {
                return Some(format!("}}{}", rest.split(' ').next().unwrap_or("")));
            }
            let k = e.split(" @").next().unwrap_or("").to_string();
            if e.ends_with('{') { Some(format!("{k}{{")) } else if e.starts_with("diag") { Some(e) } else { None }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[test]
fn if_elif_else() {
    let src = "if a:\n    x\nelif b:\n    if c: y\n    else: z\nelse:\n    w\n";
    assert_eq!(blocks(src), "If{ }If Elif{ If{ }If IfElse{ }IfElse }Elif IfElse{ }IfElse");
    let e = events(src);
    assert_eq!(e[0], "If @1:0 {");
    assert_eq!(e[1], "} If L2");
    assert_eq!(e[2], "Elif @3:0 {");
    assert_eq!(e.last().unwrap(), "} IfElse L7");
}

#[test]
fn loops_with_else() {
    let src = "for x in y:\n    break\nelse:\n    pass\nwhile a:\n    continue\nelse: pass\nasync def f():\n    async for i in g(): pass\n    async with a as b, c: pass\n";
    assert_eq!(
        blocks(src),
        "For{ }For LoopElse{ }LoopElse While{ }While LoopElse{ }LoopElse async For{ }For async With{ }With"
    );
}

#[test]
fn try_forms() {
    let src = "try:\n    a\nexcept:\n    b\ntry:\n    a\nexcept* E:\n    b\nexcept (F, G) as e:\n    c\nelse:\n    d\nfinally:\n    e\ntry: x\nfinally: y\n";
    assert_eq!(
        blocks(src),
        "Try{ }Try Except{ }Except Try{ }Try Except* (E){ }Except Except (F,G){ }Except TryElse{ }TryElse Finally{ }Finally Try{ }Try Finally{ }Finally"
    );
}

#[test]
fn with_forms() {
    let src = "with a: pass\nwith a as b, c as d:\n    pass\nwith (a as b, c):\n    pass\nwith (a, b) as c: pass\nwith (yield x): pass\n";
    assert_eq!(blocks(src), "With{ }With With{ }With With{ }With With{ }With With{ }With");
}

#[test]
fn if_conditions() {
    let src = "if TYPE_CHECKING:\n    pass\nelif typing.TYPE_CHECKING: pass\nif not TYPE_CHECKING: pass\nif __name__ == '__main__': main()\nif \"__main__\" == __name__: main()\nif TYPE_CHECKING or x: pass\nif __name__ == '__main__' and x: pass\n";
    assert_eq!(
        blocks(src),
        "If TypeChecking{ }If Elif TypeChecking{ }Elif If NotTypeChecking{ }If If MainGuard{ }If If MainGuard{ }If If{ }If If{ }If"
    );
}

#[test]
fn blocks_nested_in_defs_close_before_def_exit() {
    let src = "def f():\n    if a:\n        for b in c:\n            pass\nx = 1\n";
    assert_eq!(defs(src), "def f @1:4 L1 d0\nIf @2:4 {\nFor @3:8 {\n} For L4\n} If L4\nend def L4");
}

#[test]
fn stray_clauses_are_reported_and_body_parsed() {
    let src = "else:\n    def f(): pass\nexcept E:\n    pass\nx = 1\n";
    let e = events(src);
    assert_eq!(e[0], "diag UnexpectedToken 1:0 else");
    assert_eq!(e[1], "def f @2:8 L2 d0");
    assert_eq!(e[3], "diag UnexpectedToken 3:0 except");
    assert_eq!(e[4], "assign Plain x = other L5");
    assert_eq!(e.len(), 5);
}

/// Events whose text starts with one of `prefixes`.
fn only(src: &str, prefixes: &[&str]) -> String {
    events(src).into_iter().filter(|e| prefixes.iter().any(|p| e.starts_with(p))).collect::<Vec<_>>().join("\n")
}

#[test]
fn call_chains_and_receivers() {
    let src = "f(x)\na.b.c(1)\nself.x.m()\ncls.make()\nsuper().run()\nsuper(C, self).a.b()\nf().g()\nd[0].h()\n''.join(xs)\nf()()\n(a).b()\nself()\n";
    assert_eq!(
        only(src, &["call"]),
        "call f None 1:0\ncall a.b.c Dotted 2:0\ncall self.x.m SelfAttr 3:0\ncall cls.make ClsAttr 4:0\n\
         call super None 5:0\ncall run Super 5:8\ncall super None 6:0\ncall a.b Super 6:15\ncall f None 7:0\ncall g Unknown 7:4\n\
         call h Unknown 8:5\ncall join Unknown 9:3\ncall f None 10:0\ncall  Unknown 10:3\ncall a.b Dotted 11:1\ncall self None 12:0"
    );
}

#[test]
fn nested_calls_and_keyword_args() {
    let src = "f(g(a.b(c)), key=h(1), *args, **kw)\nx = dict(a=1, b=lambda q=2: q)\n";
    assert_eq!(
        only(src, &["call", "kw"]),
        "call f None 1:0\ncall g None 1:2\ncall a.b Dotted 1:4\nkw key\ncall h None 1:17\ncall dict None 2:4\nkw a\nkw b"
    );
}

#[test]
fn assignments() {
    let src = "x = Foo(1)\nself.y = m.Bar()\na = b = c\nx, *rest = t\n(p, q), r = z\nd[k] = v\nn += 1\nann: int = make()\nbare: str\n__all__ = ['a', \"b\"]\n__all__ += ('c',)\nt = 'a', 'b'\ns = 'abc'\nf(x)[0].attr = 1\n";
    assert_eq!(
        only(src, &["assign"]),
        "assign Plain x = call Foo L1\nassign Plain self.y = call m.Bar L2\nassign Plain a = name c L3\nassign Plain b = name c L3\n\
         assign Plain x unpacked = name t L4\nassign Plain *rest unpacked = name t L4\n\
         assign Plain p unpacked = name z L5\nassign Plain q unpacked = name z L5\nassign Plain r unpacked = name z L5\n\
         assign Augmented n += other L7\nassign Annotated ann = call make L8\nassign Annotated bare : none L9\n\
         assign Plain __all__ = strs ['a',\"b\"] L10\nassign Augmented __all__ += strs ['c'] L11\n\
         assign Plain t = strs ['a','b'] L12\nassign Plain s = other L13"
    );
    let src = "__all__ = ('a')\n__all__ = ('a',)\n__all__ = ()\n__all__ = {'a'}\n";
    assert_eq!(
        only(src, &["assign"]),
        "assign Plain __all__ = other L1\nassign Plain __all__ = strs ['a'] L2\n\
         assign Plain __all__ = strs [] L3\nassign Plain __all__ = strs ['a'] L4"
    );
}

#[test]
fn for_with_except_targets() {
    let src = "for k, v in d.items(): pass\nfor i in range(3): pass\nwith open(p) as f, g() as (a, b): pass\ntry: pass\nexcept (a.Error, OSError) as e: pass\n";
    assert_eq!(
        only(src, &["assign", "Except"]),
        "assign ForTarget k unpacked for none L1\nassign ForTarget v unpacked for none L1\nassign ForTarget i for none L2\n\
         assign AsTarget f as none L3\nassign AsTarget a unpacked as none L3\nassign AsTarget b unpacked as none L3\n\
         assign AsTarget e as none L5\nExcept (Error,OSError) @5:0 {"
    );
}

#[test]
fn walrus() {
    let src = "if (n := len(a)) > 10: pass\n[y for x in z if (y := f(x))]\n";
    assert_eq!(only(src, &["assign"]), "assign Walrus n := other L1\nassign Walrus y := other L2");
}

#[test]
fn bool_op_runs() {
    assert_eq!(only("x = a and b and c\n", &["bool"]), "bool And");
    assert_eq!(only("x = a and b or c\n", &["bool"]), "bool And\nbool Or");
    assert_eq!(only("x = a and (b or c) and d\n", &["bool"]), "bool And\nbool Or");
    assert_eq!(only("x = a or not b or c\n", &["bool"]), "bool Or");
    assert_eq!(only("f(a and b, c and d)\n", &["bool"]), "bool And\nbool And");
    assert_eq!(only("x = a and b if c and d else e and f\n", &["bool"]), "bool And\nbool And\nbool And");
    assert_eq!(only("if a and b:\n    pass\nelif c or d: pass\n", &["bool"]), "bool And\nbool Or");
}

#[test]
fn ternary_and_comprehensions() {
    assert_eq!(only("x = a if b else c\n", &["branch"]), "branch Ternary");
    assert_eq!(only("x = [a for b in c if d]\n", &["branch"]), "branch CompFor\nbranch CompIf");
    assert_eq!(
        only("x = [a if b else c for d in e if f for g in h]\n", &["branch"]),
        "branch Ternary\nbranch CompFor\nbranch CompIf\nbranch CompFor"
    );
    assert_eq!(only("x = {k: (v if v else 0) for k, v in d}\n", &["branch"]), "branch Ternary\nbranch CompFor");
    assert_eq!(only("f(x async for x in y)\n", &["branch"]), "branch CompFor");
    assert_eq!(only("x = [(a if b else c) for d in e]\n", &["branch"]), "branch Ternary\nbranch CompFor");
}

#[test]
fn lambdas_balance_and_colons() {
    let e = only("f = lambda x, y=1: x if y else lambda: y\nd = {1: lambda: 2, 3: 4}\nif lambda: 1:\n    pass\n", &["lambda", "}", "If", "assign"]);
    assert_eq!(
        e,
        "lambda {\nlambda {\n} lambda 40\n} lambda 40\nassign Plain f = other L1\nlambda {\n} lambda 58\nassign Plain d = other L2\nlambda {\n} lambda 78\nIf @3:0 {\n} If L4"
    );
    // A lambda in a header does not end the header early; slices and dicts are not headers.
    assert_eq!(defs("def f(k=lambda a, b: 0, j=1): pass\n"), "def f @1:4 L1 d0 (k=,j=)\nend def L1");
    assert_eq!(blocks("for x in a[1:2]:\n    pass\nwhile {1: 2}[k]: pass\n"), "For{ }For While{ }While");
}

#[test]
fn names_attributes_strings() {
    struct V(Vec<String>, &'static [u8]);
    impl Visitor for V {
        fn name(&mut self, t: Token) {
            self.0.push(format!("n:{}", String::from_utf8_lossy(t.text(self.1))));
        }
        fn attribute(&mut self, t: Token) {
            self.0.push(format!("a:{}", String::from_utf8_lossy(t.text(self.1))));
        }
        fn string(&mut self, t: Token) {
            self.0.push(format!("s:{}", String::from_utf8_lossy(t.text(self.1))));
        }
    }
    let src: &'static [u8] = b"x = a.b(c, 'k').d\ny = lambda p, *q: p\nz = f\"{w.v!r:>{n}}\"\n";
    let v = parse(src, V(Vec::new(), src));
    assert_eq!(v.0.join(" "), "n:x n:a a:b n:c s:'k' a:d n:y n:p n:z n:w a:v n:n");
}

#[test]
fn python2_statements() {
    let e = events("print 'hello'\nprint >>f, x\nexec code\nprint('ok')\nprint\nprint >>sys.stderr, len(x), g(y)\n");
    assert_eq!(e[0], "diag Python2 1:0 print");
    assert_eq!(e[1], "diag Python2 2:0 print");
    assert_eq!(e[2], "diag Python2 3:0 exec");
    assert_eq!(e[3], "call print None 4:0");
    // The rest of a Python 2 statement is still scanned for calls.
    assert_eq!(e[4], "diag Python2 6:0 print");
    assert_eq!(e[5], "call len None 6:20");
    assert_eq!(e[6], "call g None 6:28");
    assert_eq!(e.len(), 7);
}

#[test]
fn match_statement() {
    let src = "match command.split():\n    case [action]:\n        pass\n    case Point(x=0, y=y) if y > 0:\n        go()\n    case {'k': v, **rest}:\n        pass\n    case _:\n        pass\n";
    assert_eq!(blocks(src), "Match{ Case{ }Case Case guarded{ }Case Case{ }Case Case irrefutable{ }Case }Match");
    let e = events(src);
    assert!(e.contains(&"} Case L9 last".to_string()), "{e:?}");
    assert_eq!(e.iter().filter(|x| x.ends_with(" last")).count(), 1);
    // Class patterns are not calls; keyword patterns are not keyword args.
    assert_eq!(only(src, &["call", "kw"]), "call command.split Dotted 1:6\ncall go None 5:8");
}

#[test]
fn case_irrefutability() {
    let irr = |pat: &str| {
        let src = format!("match x:\n    case {pat}:\n        pass\n");
        let e = events(&src);
        assert!(!e.iter().any(|l| l.starts_with("diag")), "{pat}: {e:?}");
        e.iter().any(|l| l.contains("irrefutable"))
    };
    assert!(irr("_"));
    assert!(irr("y"));
    assert!(irr("(y)"));
    assert!(irr("((_))"));
    assert!(irr("1 | _"));
    assert!(irr("(1 | y)"));
    assert!(irr("_ as y"));
    assert!(irr("x as y"));
    assert!(irr("(1 | x) as y"));
    assert!(irr("1 | x as y"));
    assert!(irr("(x as y) as z"));
    assert!(irr("((_ as y))"));
    assert!(irr("1 | (x as y)"));
    assert!(!irr("1 as y"));
    assert!(!irr("[x] as y"));
    assert!(!irr("[x as y]"));
    assert!(!irr("(1 | 2) as y"));
    assert!(!irr("C(x) as y"));
    assert!(!irr("a.b"));
    assert!(!irr("1"));
    assert!(!irr("[_]"));
    assert!(!irr("(a, b)"));
    assert!(!irr("C()"));
    assert!(irr("(a) | (b, c)"));
    assert!(!irr("x,"));
    assert!(!irr("x, y"));
    assert!(!irr("(x,)"));
    assert!(!irr("x as y as"));
}

#[test]
fn case_pattern_deep_nesting() {
    // Hostile nesting must neither overflow the stack nor go quadratic.
    let n = 50_000;
    let mut alt = String::new();
    alt.push_str(&"(".repeat(n));
    alt.push('x');
    alt.push_str(&")|1".repeat(n));
    let src = format!("match v:\n    case {alt}:\n        pass\n");
    assert!(events(&src).iter().any(|l| l.contains("irrefutable")));
    let mut wrap = "(".repeat(n);
    wrap.push('1');
    wrap.push_str(&") as y".repeat(n));
    let src = format!("match v:\n    case {wrap}:\n        pass\n");
    assert!(!events(&src).iter().any(|l| l.contains("irrefutable")));
}

#[test]
fn match_as_identifier() {
    let src = "match = 1\nmatch.x = 2\nmatch(a, b)\nmatch[0]: int = 1\nprint(match)\nmatch, case = 1, 2\ncase = 3\nmatch -x:\n    case 1: pass\n";
    let e = events(src);
    assert_eq!(blocks(src), "Match{ Case{ }Case }Match", "{e:?}");
    assert!(e.contains(&"call match None 3:0".to_string()));
    assert!(e.contains(&"assign Plain match = other L1".to_string()));
}

#[test]
fn match_subject_forms() {
    assert_eq!(blocks("match (a, b):\n    case (1, 2): pass\n"), "Match{ Case{ }Case }Match");
    assert_eq!(blocks("match *a, b:\n    case _: pass\n"), "Match{ Case irrefutable{ }Case }Match");
    assert_eq!(blocks("match {1: 2}[k]:\n    case 1: pass\n"), "Match{ Case{ }Case }Match");
    assert_eq!(blocks("match lambda: 1:\n    case 1: pass\n"), "lambda {{ }lambda Match{ Case{ }Case }Match");
    // Long subject (beyond the 4-token ring buffer).
    assert_eq!(blocks("match a.b.c.d(e, f, g)[h]:\n    case 1: pass\n"), "Match{ Case{ }Case }Match");
    // Nested match inside a case body, inside a def.
    let src = "def f():\n    match x:\n        case 1:\n            match y:\n                case 2: pass\n        case _: pass\n    return 1\n";
    assert_eq!(blocks(src), "Match{ Case{ Match{ Case{ }Case }Match }Case Case irrefutable{ }Case }Match");
}

#[test]
fn type_alias_statement() {
    let src = "type X = int\ntype Pair[T] = tuple[T, T]\ntype = 1\ntype(x)\nprint(type)\n";
    assert_eq!(only(src, &["type", "assign", "call"]), "type X [] L1\ntype Pair [T] L2\nassign Plain type = other L3\ncall type None 4:0\ncall print None 5:0");
}

#[test]
fn parenthesized_with_items() {
    let src = "with (open(a) as f, open(b) as g):\n    pass\nwith (a, b) as c: pass\nwith (\n    x as y,\n    z,\n): pass\nwith (a): pass\n";
    assert_eq!(
        only(src, &["assign", "With", "diag"]),
        "assign AsTarget f as none L1\nassign AsTarget g as none L1\nWith @1:0 {\nassign AsTarget c as none L3\nWith @3:0 {\n\
         assign AsTarget y as none L5\nWith @4:0 {\nWith @8:0 {"
    );
}

/// Parses every `.py` under `$PYMAPPER_PARSE_DIR` and lists files with
/// diagnostics. Run with `cargo test -p pymapper-core --release corpus_diagnostics -- --ignored --nocapture`.
#[test]
#[ignore = "needs PYMAPPER_PARSE_DIR"]
fn corpus_diagnostics() {
    struct D(Vec<Diagnostic>, i64, i64);
    impl Visitor for D {
        fn diagnostic(&mut self, d: &Diagnostic) {
            self.0.push(*d);
        }
        fn def_enter(&mut self, _: &Def<'_>) {
            self.1 += 1;
        }
        fn def_exit(&mut self, _: DefKind, _: End) {
            self.1 -= 1;
        }
        fn block_enter(&mut self, _: &Block<'_>) {
            self.2 += 1;
        }
        fn block_exit(&mut self, _: BlockKind, _: End, _: bool) {
            self.2 -= 1;
        }
    }
    let Ok(root) = std::env::var("PYMAPPER_PARSE_DIR") else { return };
    let mut stack = vec![std::path::PathBuf::from(root)];
    let (mut files, mut bad, mut bytes) = (0, 0, 0);
    let t0 = std::time::Instant::now();
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        for e in rd.flatten() {
            let p = e.path();
            let Ok(ft) = e.file_type() else { continue };
            if ft.is_dir() {
                stack.push(p);
            } else if ft.is_file() && p.extension().is_some_and(|x| x == "py" || x == "pyi") {
                let Ok(src) = std::fs::read(&p) else { continue };
                files += 1;
                bytes += src.len();
                let d = parse(&src, D(Vec::new(), 0, 0));
                assert_eq!((d.1, d.2), (0, 0), "unbalanced events in {}", p.display());
                if let Some(first) = d.0.first() {
                    bad += 1;
                    let t = String::from_utf8_lossy(first.token.text(&src));
                    println!("{}:{}:{}: {:?} {t:?} (+{})", p.display(), first.pos.line, first.pos.col, first.kind, d.0.len() - 1);
                }
            }
        }
    }
    println!("{files} files, {bytes} bytes, {bad} with diagnostics, {:?}", t0.elapsed());
}

#[test]
fn line_positions_for_all_line_endings() {
    for nl in ["\n", "\r\n", "\r"] {
        let src = format!("x = 1{nl}@d{nl}def f():{nl}    g(){nl}    '''a{nl}b'''{nl}    h(){nl}");
        let e = events(&src);
        assert_eq!(e[1], "dec d", "{nl:?}");
        assert_eq!(e[2], "def f @3:4 L2 d0 decs=1", "{nl:?} {e:?}");
        assert_eq!(e[3], "call g None 4:4", "{nl:?}");
        assert_eq!(e[4], "call h None 7:4", "{nl:?}");
        assert_eq!(e[5], "end def L7", "{nl:?}");
    }
}

/// Tiny xorshift RNG for the randomized tests.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

#[test]
fn line_cursor_matches_naive_count() {
    use super::lines::LineCursor;
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    for round in 0..600 {
        let len = rng.below(if round % 3 == 0 { 2000 } else { 200 });
        let alphabet: &[u8] = match rng.below(3) {
            0 => b"ab\n\n ",
            1 => b"a\r\n\r",
            // Mostly `\n`, a rare `\r` late in the file (switches to the byte-wise path).
            _ => b"abcdefghij\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\r",
        };
        let src: Vec<u8> = (0..len).map(|_| alphabet[rng.below(alphabet.len())]).collect();
        let mut lc = LineCursor::new(&src);
        let mut last = rng.below(len + 1) as u32;
        for _ in 0..40 {
            // Mix nearby forward/backward queries with long jumps.
            let off = match rng.below(4) {
                0 => rng.below(len + 1) as u32,
                1 => last.saturating_sub(rng.below(5) as u32),
                _ => (last + rng.below(30) as u32).min(len as u32 + 3),
            };
            last = off.min(len as u32);
            let got = lc.pos(off);
            // Naive: lines end at `\n`, `\r\n` (once) and lone `\r`.
            let off = off.min(len as u32);
            let (mut line, mut start) = (1u32, 0u32);
            for i in 0..off as usize {
                let end = src[i] == b'\n' || (src[i] == b'\r' && src.get(i + 1) != Some(&b'\n'));
                if end {
                    line += 1;
                    start = i as u32 + 1;
                }
            }
            assert_eq!((got.line, got.col), (line, off - start), "{src:?} @{off}");
        }
    }
}

#[test]
fn recovery_unclosed_bracket_before_def() {
    let src = "x = f(a,\ndef g():\n    return 1\ny = 2\n";
    let e = events(src);
    assert_eq!(e[1], "diag UnclosedBracket 2:0 def", "{e:?}");
    assert!(e.contains(&"def g @2:4 L2 d0".to_string()), "{e:?}");
    assert!(e.contains(&"end def L3".to_string()), "{e:?}");
    assert!(e.contains(&"assign Plain y = other L4".to_string()), "{e:?}");
}

#[test]
fn recovery_unclosed_bracket_in_headers() {
    let src = "def f(a, b:\n    pass\nclass C(Base:\n    pass\nfrom m import (a, b\nimport os\n";
    let e = defs(src);
    assert!(e.contains("def f"), "{e}");
    assert!(e.contains("class C"), "{e}");
    assert!(e.contains("import os L6"), "{e}");
    assert!(e.contains("diag"), "{e}");
}

#[test]
fn recovery_missing_colon_and_indent() {
    let e = events("if x\n    y = 1\nwhile True\n    pass\n");
    assert!(e[0].starts_with("diag ExpectedColon 1:4"), "{e:?}");
    assert!(e.contains(&"assign Plain y = other L2".to_string()), "{e:?}");
    assert_eq!(blocks("if x\n    pass\n"), "diag ExpectedColon 1:4 \n If{ }If");
    assert_eq!(blocks("if x:\npass\n"), "If{ diag ExpectedIndent 2:0 pass }If");
    assert_eq!(blocks("for x in y:"), "For{ diag ExpectedIndent 1:11  }For");
}

#[test]
fn recovery_unexpected_indent_and_brackets() {
    let e = events("x = 1\n    y = 2\nz = 3)\n");
    assert!(e.contains(&"diag UnexpectedIndent 2:4 ".to_string()) || e.iter().any(|l| l.starts_with("diag UnexpectedIndent 2:")), "{e:?}");
    assert!(e.contains(&"assign Plain y = other L2".to_string()), "{e:?}");
    assert!(e.iter().any(|l| l.starts_with("diag UnmatchedBracket 3:")), "{e:?}");
    assert!(e.contains(&"assign Plain z = other L3".to_string()), "{e:?}");
}

#[test]
fn recovery_lexer_errors_and_bad_names() {
    let e = events("x = $\ndef (a): pass\nclass : pass\nimport\n@dec\nx = 1\ns = 'abc\ny = 2\n");
    assert!(e.iter().any(|l| l.starts_with("diag Lex 1:4")), "{e:?}");
    assert!(e.iter().any(|l| l.starts_with("diag ExpectedName 2:4")), "{e:?}");
    assert!(e.iter().any(|l| l.starts_with("diag ExpectedName 3:6")), "{e:?}");
    assert!(e.iter().any(|l| l.starts_with("diag ExpectedName 4:")), "{e:?}");
    assert!(e.iter().any(|l| l.starts_with("diag UnexpectedToken 6:0")), "{e:?}");
    assert!(e.iter().any(|l| l.starts_with("diag Lex 7:4")), "{e:?}");
    assert!(e.contains(&"assign Plain y = other L8".to_string()), "{e:?}");
}

#[test]
fn recovery_inconsistent_dedent() {
    let src = "def f():\n        x = 1\n    y = 2\nz = 3\n";
    let e = events(src);
    assert!(e.iter().any(|l| l.starts_with("diag")), "{e:?}");
    assert!(e.contains(&"assign Plain z = other L4".to_string()), "{e:?}");
}

/// Checks event balance and span sanity for arbitrary input.
struct Check<'s> {
    src: &'s [u8],
    defs: i64,
    blocks: Vec<BlockKind>,
    lambdas: i64,
    last_token_end: u32,
}

impl Visitor for Check<'_> {
    fn token(&mut self, t: Token) {
        assert!(t.end() as usize <= self.src.len());
        assert!(t.start >= self.last_token_end, "tokens out of order");
        self.last_token_end = t.end();
    }
    fn def_enter(&mut self, d: &Def<'_>) {
        assert!(d.start <= d.name.start);
        self.defs += 1;
    }
    fn def_exit(&mut self, _: DefKind, _: End) {
        self.defs -= 1;
        assert!(self.defs >= 0);
    }
    fn block_enter(&mut self, b: &Block<'_>) {
        self.blocks.push(b.kind);
    }
    fn block_exit(&mut self, kind: BlockKind, _: End, _: bool) {
        assert_eq!(self.blocks.pop(), Some(kind));
    }
    fn lambda_enter(&mut self, _: Token) {
        self.lambdas += 1;
    }
    fn lambda_exit(&mut self, _: u32) {
        self.lambdas -= 1;
        assert!(self.lambdas >= 0);
    }
    fn call(&mut self, c: &Call<'_>) {
        for t in c.names {
            assert!(t.end() as usize <= self.src.len());
        }
    }
    fn assign(&mut self, a: &Assign<'_>) {
        assert!(!a.target.is_empty());
    }
}

fn check(src: &[u8]) {
    let c = parse(src, Check { src, defs: 0, blocks: Vec::new(), lambdas: 0, last_token_end: 0 });
    assert_eq!(c.defs, 0, "{:?}", String::from_utf8_lossy(src));
    assert!(c.blocks.is_empty(), "{:?}", String::from_utf8_lossy(src));
    assert_eq!(c.lambdas, 0, "{:?}", String::from_utf8_lossy(src));
}

#[test]
fn fuzz_fragment_soup_is_balanced() {
    const FRAGS: &[&str] = &[
        "def ", "class ", "async ", "f", "x", "self", "super", "match ", "case ", "type ", "_", "lambda", " ", "\n",
        "    ", "\t", ":", "(", ")", "[", "]", "{", "}", ",", ".", "=", ":=", "+=", "*", "**", "@", "->", "if ",
        "elif ", "else", "for ", " in ", "while ", "try", "except", "finally", "with ", " as ", "return ", "import ",
        "from ", "pass", "global ", "'s'", "f'{x}'", "\"\"\"", "1", "and ", "or ", "not ", "|", ";", "\\\n", "#c\n",
        "yield ", "await ", "del ", "raise ", "assert ", "nonlocal ", "...", "!", "$", "print ",
    ];
    let mut rng = Rng(0x2545_f491_4f6c_dd1d);
    for _ in 0..20_000 {
        let n = rng.below(40);
        let mut s = String::new();
        for _ in 0..n {
            s.push_str(FRAGS[rng.below(FRAGS.len())]);
        }
        check(s.as_bytes());
    }
}

#[test]
fn fuzz_random_bytes_and_mutated_code() {
    let mut rng = Rng(0xdead_beef_cafe_f00d);
    for _ in 0..5_000 {
        let n = rng.below(64);
        let v: Vec<u8> = (0..n).map(|_| rng.next() as u8).collect();
        check(&v);
    }
    let base = b"@d(1)\nclass C(B, metaclass=M):\n    def m(self, a=1, *b, **c) -> int:\n        match a:\n            case [x, *_] if x: return (lambda: x)()\n            case _: pass\n        with (open(a) as f, g):\n            for i, j in f: yield {k: v for k, v in i}\n        try:\n            pass\n        except* E as e:\n            raise\n        finally:\n            del a\ntype T[X] = list[X]\n";
    for _ in 0..5_000 {
        let mut v = base.to_vec();
        for _ in 0..1 + rng.below(4) {
            let i = rng.below(v.len());
            match rng.below(3) {
                0 => {
                    v.remove(i);
                }
                1 => v.insert(i, b"()[]{}:\n \t'\"#\\,=@"[rng.below(17)]),
                _ => v[i] = rng.next() as u8,
            }
        }
        check(&v);
    }
}

#[test]
fn deep_nesting_does_not_overflow() {
    let mut s = String::new();
    for i in 0..120 {
        s.push_str(&" ".repeat(i));
        s.push_str("if x:\n");
    }
    s.push_str(&" ".repeat(120));
    s.push_str("pass\n");
    check(s.as_bytes());
    let brackets = "(".repeat(100_000) + &")".repeat(100_000) + "\n";
    check(brackets.as_bytes());
    let lambdas = "x = ".to_string() + &"lambda: ".repeat(10_000) + "1\n";
    check(lambdas.as_bytes());
}

#[test]
fn except_without_parentheses_pep758() {
    let src = "try:\n    pass\nexcept ValueError, mod.TypeError:\n    pass\nexcept* A, B:\n    pass\n";
    assert_eq!(only(src, &["Except", "diag"]), "Except (ValueError,TypeError) @3:0 {\nExcept* (A,B) @5:0 {");
}

#[test]
fn exclamation_outside_fstring_and_suppressions() {
    assert_eq!(only("x = a ! b\ny = f'{a!r}' + t'{b!s:>3}'\n", &["diag"]), "diag UnexpectedToken 1:6 !");
    let src = b"x = 1  # pymapper: ignore[PM101]\nmatch y:  # pymapper: ignore[PM2]\n    case _: pass\n";
    let mut p = super::Parser::new(src, NoopVisitor);
    p.parse_file();
    let (_, sup) = p.finish();
    assert_eq!(sup.len(), 2);
    assert_eq!(super::parse_with_suppressions(src, NoopVisitor).1, sup);
}

#[test]
fn recovery_does_not_duplicate_suppressions() {
    let src = b"x = f(a,\ndef g(): pass  # pymapper: ignore[PM1]\n";
    let mut p = super::Parser::new(src, NoopVisitor);
    p.parse_file();
    assert_eq!(p.diagnostics(), 1);
    let (_, sup) = p.finish();
    assert_eq!(sup.len(), 1);
}

#[test]
fn long_single_line_scales_linearly() {
    // Call positions move the line cursor past assign targets (`a=f()`), so
    // queries go backwards on the same line; that must not rescan the line.
    let n = 100_000;
    let one = "a=f();".repeat(n) + "\n";
    let many = "a=f()\n".repeat(n);
    let time = |s: &str| {
        let mut best = std::time::Duration::MAX;
        for _ in 0..3 {
            let t = std::time::Instant::now();
            let mut p = super::Parser::new(s.as_bytes(), NoopVisitor);
            p.parse_file();
            assert_eq!(p.diagnostics(), 0);
            best = best.min(t.elapsed());
        }
        best
    };
    let (t1, tn) = (time(&one), time(&many));
    // Quadratic behaviour is ~1000x slower at this size; allow generous noise.
    assert!(t1 < tn * 5 + std::time::Duration::from_millis(50), "one line {t1:?} vs separate lines {tn:?}");
}

#[test]
fn more_than_u16_nested_lambdas_balance() {
    struct Count(u32, u32);
    impl Visitor for Count {
        fn lambda_enter(&mut self, _: Token) {
            self.0 += 1;
        }
        fn lambda_exit(&mut self, _: u32) {
            self.1 += 1;
        }
    }
    let n = 70_000;
    let src = "x = ".to_string() + &"lambda: ".repeat(n) + "1\n";
    let c = parse(src.as_bytes(), Count(0, 0));
    assert_eq!((c.0, c.1), (n as u32, n as u32));
    // Without the `:` (pending lambdas) as well.
    let src = "x = ".to_string() + &"lambda ".repeat(n) + "\n";
    let c = parse(src.as_bytes(), Count(0, 0));
    assert_eq!(c.0, c.1);
    check(src.as_bytes());
}

#[test]
fn recovery_unclosed_bracket_at_eof() {
    let e = only("x = 1\nfoo(a,\n  b\n", &["diag"]);
    assert_eq!(e.lines().count(), 1, "{e}");
    assert!(e.starts_with("diag UnclosedBracket"), "{e}");
    let e = only("x = f(\nif a:\n    b = g()", &["diag"]);
    assert_eq!(e.lines().count(), 1, "{e}");
    assert!(e.starts_with("diag UnclosedBracket"), "{e}");
    let e = only("for x in [1,\n  2:\n    pass\n", &["diag"]);
    assert!(e.starts_with("diag UnclosedBracket"), "{e}");
    // Inside a def header.
    let e = only("def f(a, b=(1,\n", &["diag"]);
    assert_eq!(e.lines().filter(|l| l.starts_with("diag UnclosedBracket")).count(), 1, "{e}");
    // Recovered at a statement keyword: reported once, not again at EOF.
    let mut p = super::Parser::new(b"x = f(a,\ndef g(): pass\n", NoopVisitor);
    p.parse_file();
    assert_eq!(p.diagnostics(), 1);
    // Closed brackets report nothing.
    assert_eq!(only("x = (1,\n 2)\n", &["diag"]), "");
}

#[test]
fn recovery_reprocesses_indentation_of_keyword_line() {
    // `def g` at column 0 closes `f` (DEDENT) instead of nesting into it.
    let src = "def f():\n    x = foo(\ndef g():\n    pass\n";
    let e = events(src);
    assert!(e.contains(&"def g @3:4 L3 d0".to_string()), "{e:?}");
    assert_eq!(e.iter().filter(|l| l.starts_with("diag")).count(), 1, "{e:?}");
    // Deeper keyword line: an unexpected indent, then the statement.
    let e = events("x = foo(\n    return 1\ny = 2\n");
    assert!(e.iter().any(|l| l.starts_with("diag UnclosedBracket 2:4")), "{e:?}");
    assert!(e.contains(&"assign Plain y = other L3".to_string()), "{e:?}");
    // Keyword mid-line: indentation is not reprocessed.
    let e = events("def f():\n    x = foo(a, b); return 1\n    y = 2\n");
    assert!(e.contains(&"assign Plain y = other L3".to_string()), "{e:?}");
    let e = events("def f():\n    x = foo(\ny = 1\ndef g():\n    pass");
    assert!(e.contains(&"def g @4:4 L4 d0".to_string()), "{e:?}");
    check(b"def f():\n    if a:\n        x = foo(\ndef g():\n    pass\n");
    check(b"class C:\n    x = [\n        def m(self): pass\n  return\n");
}

#[test]
fn parenthesised_primaries_are_transparent_in_chains() {
    // As in the AST: parentheses around a whole primary do not change the chain.
    let src = "((a.b)).c()\n(x)()\n(self).m()\n(super()).m()\n(f().g).h()\n(d[0]).k()\n((f)(1)).z()\n";
    assert_eq!(
        only(src, &["call"]),
        "call a.b.c Dotted 1:2\ncall x None 2:1\ncall self.m SelfAttr 3:1\ncall super None 4:1\ncall m Super 4:10\n\
         call f None 5:1\ncall g.h Unknown 5:5\ncall k Unknown 6:7\ncall f None 7:2\ncall z Unknown 7:9"
    );
    // Anything else in the group makes it an unknown receiver.
    let src = "(a + b).c()\n(a, b).c()\n(f\"{a}\").x()\n(not a).y()\n(lambda: a).z()\n(a if b else c).w()\n(await a).v()\n";
    assert_eq!(
        only(src, &["call"]),
        "call c Unknown 1:8\ncall c Unknown 2:7\ncall x Unknown 3:9\ncall y Unknown 4:8\ncall z Unknown 5:12\n\
         call w Unknown 6:16\ncall v Unknown 7:10"
    );
}
