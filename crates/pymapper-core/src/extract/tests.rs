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

fn ev(src: &str) -> String {
    events(src).join("\n")
}

#[test]
#[ignore = "needs expression shapes"]
fn defs_and_nesting() {
    let src = "def f(a, b=1, *args, c, d: int = 2, **kw) -> int:\n    def g():\n        pass\n    return 1\nclass C(Base, m.Mixin, metaclass=M):\n    x = 1\n    async def h(self, /, x): ...\n";
    assert_eq!(
        ev(src),
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
#[ignore = "needs expression shapes"]
fn decorators_and_type_params() {
    let src = "@property\n@app.route('/x', methods=['GET'])\n@(lambda f: f)\ndef f[T: int, *Ts, **P = int](x: T) -> T: return x\n\n@dataclass\nclass P[T](Generic[T]): pass\n";
    assert_eq!(
        ev(src),
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
    assert_eq!(ev("def f(a, /, b, *, c=1):\n  pass\n"), "def f @1:4 L1 d0 (/a,b,kw:c=)\nend def L2");
    assert_eq!(ev("lambda_ = 1\ndef f(\n  a,\n  b,\n):\n  pass\n"), "def f @2:4 L2 d0 (a,b)\nend def L6");
}

#[test]
fn imports() {
    let src = "import os, a.b.c as d\nfrom . import x\nfrom ..pkg.mod import (y as z,\n   w,)\nfrom ... import *\nfrom m import *\n";
    assert_eq!(
        ev(src),
        "import os L1\nimport a.b.c as d L1\nfrom . : x L2\nfrom ..pkg.mod : y as z L3\nfrom ..pkg.mod : w L4\nfrom ... : * L5\nfrom m : * L6"
    );
}

#[test]
fn decls_and_simple_statements() {
    let src = "def f():\n    global a, b\n    nonlocal c\n    del x; return; pass\n    raise E from e\n    assert x, 'm'\n    break\n    continue\n";
    assert_eq!(ev(src), "def f @1:4 L1 d0\nGlobal a,b\nNonlocal c\nend def L8");
}

#[test]
fn async_def_and_single_line_bodies() {
    let src = "async def f(): await x\nclass E: pass\ndef g(): x = 1; y = 2\n";
    assert_eq!(
        ev(src),
        "async def f @1:10 L1 d0\nend def L1\nclass E @2:6 L2 d0\nend class L2\ndef g @3:4 L3 d0\nend def L3"
    );
}

#[test]
fn def_end_line_with_trailing_comments_and_blank_lines() {
    let src = "def f():\n    x = '''a\nb'''\n\n    # c\n\nz = 1\n";
    assert_eq!(ev(src), "def f @1:4 L1 d0\nend def L3");
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
        "Try{ }Try Except{ }Except Try{ }Try Except*{ }Except Except{ }Except TryElse{ }TryElse Finally{ }Finally Try{ }Try Finally{ }Finally"
    );
}

#[test]
fn with_forms() {
    let src = "with a: pass\nwith a as b, c as d:\n    pass\nwith (a as b, c):\n    pass\nwith (a, b) as c: pass\nwith (yield x): pass\n";
    assert_eq!(blocks(src), "With{ }With With{ }With With{ }With With{ }With With{ }With");
}

#[test]
#[ignore = "needs expression shapes"]
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
    assert_eq!(ev(src), "def f @1:4 L1 d0\nIf @2:4 {\nFor @3:8 {\n} For L4\n} If L4\nend def L4");
}

#[test]
fn stray_clauses_are_reported_and_body_parsed() {
    let src = "else:\n    def f(): pass\nexcept E:\n    pass\nx = 1\n";
    let e = events(src);
    assert_eq!(e[0], "diag UnexpectedToken 1:0 else");
    assert_eq!(e[1], "def f @2:8 L2 d0");
    assert_eq!(e[3], "diag UnexpectedToken 3:0 except");
    assert_eq!(e.len(), 4);
}
