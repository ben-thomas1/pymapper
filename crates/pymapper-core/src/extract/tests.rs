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
         call h Unknown 8:5\ncall join Unknown 9:3\ncall f None 10:0\ncall  Unknown 10:3\ncall b Unknown 11:4\ncall self None 12:0"
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
    let e = events("print 'hello'\nprint >>f, x\nexec code\nprint('ok')\nprint\n");
    assert_eq!(e[0], "diag Python2 1:0 print");
    assert_eq!(e[1], "diag Python2 2:0 print");
    assert_eq!(e[2], "diag Python2 3:0 exec");
    assert_eq!(e[3], "call print None 4:0");
    assert_eq!(e.len(), 4);
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
    assert!(!irr("_ as y"));
    assert!(!irr("a.b"));
    assert!(!irr("1"));
    assert!(!irr("[_]"));
    assert!(!irr("(a, b)"));
    assert!(!irr("C()"));
    assert!(irr("(a) | (b, c)"));
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
