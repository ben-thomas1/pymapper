//! The ruff side of `diff-ruff`: parse with `ruff_python_parser` and normalise into
//! [`FileFacts`] following the conventions documented in [`super::facts`].

use std::path::Path;

use ruff_python_ast::visitor::{self, Visitor};
use ruff_python_ast::{self as ast, ExceptHandler, Expr, Stmt};
use ruff_text_size::{Ranged, TextRange};

use super::facts::{Call, Def, DefKind, Extractor, FactSource, FileFacts, Import, LineIndex, MODULE_SCOPE, Skip, SkipKind, Str, normalise_expr};

/// Ruff parser as the reference fact source. Stateless: ruff allocates a fresh AST per file,
/// so it is its own (zero-sized) [`Extractor`].
pub struct RuffOracle;

impl FactSource for RuffOracle {
    fn name(&self) -> &'static str {
        "ruff"
    }

    fn extractor(&self) -> Box<dyn Extractor + '_> {
        Box::new(RuffOracle)
    }
}

impl Extractor for RuffOracle {
    fn extract(&mut self, _path: &Path, src: &[u8], lines: &LineIndex, out: &mut FileFacts) -> Result<(), Skip> {
        let text = std::str::from_utf8(src).map_err(|e| Skip {
            kind: SkipKind::NotUtf8,
            line: lines.line(e.valid_up_to() as u32),
            message: "source is not valid UTF-8".to_owned(),
        })?;
        let parsed = ruff_python_parser::parse_module(text).map_err(|e| Skip {
            kind: SkipKind::Syntax,
            line: lines.line(e.location.start().to_u32()),
            message: e.error.to_string(),
        })?;
        let mut c = Collector { src, lines, out, scope: MODULE_SCOPE };
        c.visit_body(parsed.suite());
        Ok(())
    }
}

struct Collector<'s> {
    src: &'s [u8],
    lines: &'s LineIndex,
    out: &'s mut FileFacts,
    scope: u32,
}

impl Collector<'_> {
    #[inline]
    fn line(&self, range: TextRange) -> u32 {
        self.lines.line(range.start().to_u32())
    }

    /// Pushes a def record (qualname = parent qualname + "." + name) and returns its index.
    fn push_def(&mut self, kind: DefKind, name: &ast::Identifier) -> u32 {
        let start = self.out.begin_str();
        if self.scope != MODULE_SCOPE {
            let parent = self.out.defs[self.scope as usize].qualname;
            let r = parent.off as usize..(parent.off + parent.len) as usize;
            self.out.text.extend_from_within(r);
            self.out.text.push(b'.');
        }
        self.out.text.extend_from_slice(name.id.as_bytes());
        let qualname = self.out.end_str(start);
        let idx = self.out.defs.len() as u32;
        self.out.defs.push(Def {
            kind,
            parent: self.scope,
            qualname,
            line: self.line(name.range),
            decorators: Str::default(),
            bases: Str::default(),
            params: 0,
            mccabe: 0,
        });
        idx
    }

    fn decorators(&mut self, list: &[ast::Decorator]) -> Str {
        let start = self.out.begin_str();
        for (k, d) in list.iter().enumerate() {
            // The decorator's own range starts at `@` and, unlike the expression's, keeps
            // the parentheses of a parenthesised expression (`@(a.b)`).
            let text = slice(self.src, d.range());
            if k > 0 {
                self.out.text.push(b' ');
            }
            self.out.text.push(b'@');
            normalise_expr(&text[1..], &mut self.out.text);
        }
        self.out.end_str(start)
    }

    fn function(&mut self, f: &ast::StmtFunctionDef) {
        let kind = if f.is_async { DefKind::AsyncFunction } else { DefKind::Function };
        let idx = self.push_def(kind, &f.name);
        let decorators = self.decorators(&f.decorator_list);
        {
            let d = &mut self.out.defs[idx as usize];
            d.decorators = decorators;
            d.params = f.parameters.len() as u32;
            d.mccabe = get_complexity_number(&f.body) as u32 + 1;
        }
        // Evaluated in the enclosing scope.
        for d in &f.decorator_list {
            self.visit_decorator(d);
        }
        if let Some(tp) = &f.type_params {
            self.visit_type_params(tp);
        }
        self.visit_parameters(&f.parameters);
        if let Some(r) = &f.returns {
            self.visit_annotation(r);
        }
        let saved = std::mem::replace(&mut self.scope, idx);
        self.visit_body(&f.body);
        self.scope = saved;
    }

    fn class(&mut self, c: &ast::StmtClassDef) {
        let idx = self.push_def(DefKind::Class, &c.name);
        let decorators = self.decorators(&c.decorator_list);
        let start = self.out.begin_str();
        if let Some(args) = &c.arguments {
            // Everything between the class's `(` and `)`, so parenthesised bases keep their
            // parentheses; a trailing comma is not part of the fact.
            let text = slice(self.src, args.range());
            normalise_expr(&text[1..text.len() - 1], &mut self.out.text);
            if self.out.text.len() > start as usize && self.out.text.last() == Some(&b',') {
                self.out.text.pop();
            }
        }
        let bases = self.out.end_str(start);
        {
            let d = &mut self.out.defs[idx as usize];
            d.decorators = decorators;
            d.bases = bases;
        }
        for d in &c.decorator_list {
            self.visit_decorator(d);
        }
        if let Some(tp) = &c.type_params {
            self.visit_type_params(tp);
        }
        if let Some(args) = &c.arguments {
            self.visit_arguments(args);
        }
        let saved = std::mem::replace(&mut self.scope, idx);
        self.visit_body(&c.body);
        self.scope = saved;
    }

    fn import(&mut self, module: &[u8], level: u32, names: &[ast::Alias], from: bool) {
        for alias in names {
            let line = self.line(alias.range);
            let (module, name) = if from {
                (self.out.push_str(module), self.out.push_str(alias.name.id.as_bytes()))
            } else {
                (self.out.push_str(alias.name.id.as_bytes()), Str::default())
            };
            let alias_str = match &alias.asname {
                Some(a) => self.out.push_str(a.id.as_bytes()),
                None => Str::default(),
            };
            self.out.imports.push(Import { scope: self.scope, module, name, alias: alias_str, level, line });
        }
    }

    fn call(&mut self, call: &ast::ExprCall) {
        let start = self.out.begin_str();
        push_callee(&call.func, &mut self.out.text);
        let callee = self.out.end_str(start);
        let at = chain_start(&call.func).unwrap_or(call.arguments.range.start());
        let line = self.lines.line(at.to_u32());
        self.out.calls.push(Call { scope: self.scope, callee, line });
    }
}

#[inline]
fn slice(src: &[u8], range: TextRange) -> &[u8] {
    &src[range.start().to_usize()..range.end().to_usize()]
}

/// Start of the first name of the callee chain (see the facts module conventions): the root
/// name, else the first attribute after a `super()` / unknown receiver; `None` for an empty
/// chain.
fn chain_start(func: &Expr) -> Option<ruff_text_size::TextSize> {
    match func {
        Expr::Name(n) => Some(n.range.start()),
        Expr::Attribute(a) => chain_start(&a.value).or(Some(a.attr.range.start())),
        _ => None,
    }
}

/// Writes the receiver chain of a call's `func` (see the facts module conventions).
fn push_callee(func: &Expr, out: &mut Vec<u8>) {
    match func {
        Expr::Name(n) => out.extend_from_slice(n.id.as_bytes()),
        Expr::Attribute(a) => {
            push_callee(&a.value, out);
            out.push(b'.');
            out.extend_from_slice(a.attr.id.as_bytes());
        }
        Expr::Call(c) if matches!(&*c.func, Expr::Name(n) if n.id.as_str() == "super") => {
            out.extend_from_slice(b"super()");
        }
        _ => out.push(b'?'),
    }
}

impl<'a> Visitor<'a> for Collector<'_> {
    fn visit_stmt(&mut self, stmt: &'a Stmt) {
        match stmt {
            Stmt::FunctionDef(f) => self.function(f),
            Stmt::ClassDef(c) => self.class(c),
            Stmt::Import(i) => self.import(b"", 0, &i.names, false),
            Stmt::ImportFrom(i) => {
                let module = i.module.as_ref().map_or(&b""[..], |m| m.id.as_bytes());
                self.import(module, i.level, &i.names, true);
            }
            _ => visitor::walk_stmt(self, stmt),
        }
    }

    fn visit_expr(&mut self, expr: &'a Expr) {
        if let Expr::Call(call) = expr {
            self.call(call);
        }
        visitor::walk_expr(self, expr);
    }
}

/// Direct port of ruff's `get_complexity_number`
/// (`crates/ruff_linter/src/rules/mccabe/rules/function_is_too_complex.rs`, tag 0.16.10).
/// A function's C901 score is `get_complexity_number(body) + 1`. Kept verbatim (including
/// re-walking nested bodies) so the oracle is obviously faithful; nesting depth is small.
pub fn get_complexity_number(stmts: &[Stmt]) -> usize {
    let mut complexity = 0;
    for stmt in stmts {
        match stmt {
            Stmt::If(ast::StmtIf { body, elif_else_clauses, .. }) => {
                complexity += 1;
                complexity += get_complexity_number(body);
                for clause in elif_else_clauses {
                    if clause.test.is_some() {
                        complexity += 1;
                    }
                    complexity += get_complexity_number(&clause.body);
                }
            }
            Stmt::For(ast::StmtFor { body, orelse, .. }) => {
                complexity += 1;
                complexity += get_complexity_number(body);
                complexity += get_complexity_number(orelse);
            }
            Stmt::With(ast::StmtWith { body, .. }) => {
                complexity += get_complexity_number(body);
            }
            Stmt::While(ast::StmtWhile { body, orelse, .. }) => {
                complexity += 1;
                complexity += get_complexity_number(body);
                complexity += get_complexity_number(orelse);
            }
            Stmt::Match(ast::StmtMatch { cases, .. }) => {
                for case in cases {
                    complexity += 1;
                    complexity += get_complexity_number(&case.body);
                }
                if let Some(last_case) = cases.last() {
                    // An irrefutable last case is like the `else` of an `if`.
                    if last_case.guard.is_none() && last_case.pattern.is_irrefutable() {
                        complexity -= 1;
                    }
                }
            }
            Stmt::Try(ast::StmtTry { body, handlers, orelse, finalbody, .. }) => {
                complexity += get_complexity_number(body);
                if !orelse.is_empty() {
                    complexity += 1;
                }
                complexity += get_complexity_number(orelse);
                complexity += get_complexity_number(finalbody);
                for handler in handlers {
                    complexity += 1;
                    let ExceptHandler::ExceptHandler(ast::ExceptHandlerExceptHandler { body, .. }) = handler;
                    complexity += get_complexity_number(body);
                }
            }
            Stmt::FunctionDef(ast::StmtFunctionDef { body, .. }) => {
                complexity += 1;
                complexity += get_complexity_number(body);
            }
            Stmt::ClassDef(ast::StmtClassDef { body, .. }) => {
                complexity += get_complexity_number(body);
            }
            _ => {}
        }
    }
    complexity
}

#[cfg(test)]
mod tests {
    use super::super::facts::{Bytes, MODULE_SCOPE_NAME};
    use super::*;

    fn facts(src: &str) -> FileFacts {
        let mut lines = LineIndex::default();
        lines.reset(src.as_bytes());
        let mut out = FileFacts::default();
        RuffOracle.extract(Path::new("t.py"), src.as_bytes(), &lines, &mut out).expect("parses");
        out
    }

    fn s(f: &FileFacts, x: Str) -> String {
        Bytes(f.str(x)).to_string()
    }

    /// Defs rendered as "kind qualname line params mccabe decorators | bases".
    fn defs(f: &FileFacts) -> Vec<String> {
        f.defs
            .iter()
            .map(|d| {
                format!(
                    "{} {} {} {} {} {}|{}",
                    d.kind.as_str(),
                    s(f, d.qualname),
                    d.line,
                    d.params,
                    d.mccabe,
                    s(f, d.decorators),
                    s(f, d.bases)
                )
            })
            .collect()
    }

    fn calls(f: &FileFacts) -> Vec<String> {
        let mut v: Vec<String> =
            f.calls.iter().map(|c| format!("{} {} {}", Bytes(f.scope_name(c.scope)), s(f, c.callee), c.line)).collect();
        v.sort();
        v
    }

    fn imports(f: &FileFacts) -> Vec<String> {
        f.imports
            .iter()
            .map(|i| {
                format!(
                    "{} {}|{}|{}|{} {}",
                    Bytes(f.scope_name(i.scope)),
                    s(f, i.module),
                    s(f, i.name),
                    s(f, i.alias),
                    i.level,
                    i.line
                )
            })
            .collect()
    }

    /// C901 score of the first function in `src`.
    fn cc(src: &str) -> u32 {
        facts(src).defs.iter().find(|d| d.kind != DefKind::Class).expect("a function").mccabe
    }

    #[test]
    fn defs_qualnames_lines_and_texts() {
        let f = facts(
            "@dataclass(\n    frozen=True,  # c\n)\nclass A(Base, metaclass = Meta):\n    @property\n    def x(self): ...\n    @x.setter\n    def x(self, v, /, *a, k=1, **kw): ...\n    async def g():\n        def inner(): pass\n",
        );
        assert_eq!(
            defs(&f),
            [
                "class A 4 0 0 @dataclass(frozen=True,)|Base,metaclass=Meta",
                "function A.x 6 1 1 @property|",
                "function A.x 8 5 1 @x.setter|",
                "async-function A.g 9 0 2 |",
                "function A.g.inner 10 0 1 |",
            ]
        );
        assert_eq!(f.defs[1].parent, 0);
        assert_eq!(f.defs[4].parent, 3);
        assert_eq!(f.defs[0].parent, MODULE_SCOPE);
    }

    #[test]
    fn class_bases_starred_and_kwargs() {
        let f = facts("class C(*bs, **kw): pass\nclass D: pass\nclass E(): pass\n");
        assert_eq!(defs(&f), ["class C 1 0 0 |*bs,**kw", "class D 2 0 0 |", "class E 3 0 0 |"]);
    }

    #[test]
    fn parenthesised_decorators_and_bases_keep_parens() {
        let f = facts(
            "@ (a.b)  # c\n@(x)(y)\ndef f(): pass\nclass G((B), (C)[int],\n    # note\n    metaclass=(M),\n): pass\nclass H(B,): pass\nclass I(f(','),): pass\n",
        );
        assert_eq!(
            defs(&f),
            [
                "function f 3 0 1 @(a.b) @(x)(y)|",
                "class G 4 0 0 |(B),(C)[int],metaclass=(M)",
                "class H 8 0 0 |B",
                "class I 9 0 0 |f(',')",
            ]
        );
    }

    #[test]
    fn decorator_boundaries_and_padded_bases() {
        // One decorator `a @ b` vs two decorators; bases with leading whitespace must not be
        // glued to the previous fact's text (the qualname `A` / decorators end in word bytes).
        let f = facts("@a @ b\ndef f(): pass\n@a\n@b\ndef g(): pass\nclass A( B ): pass\n@d\nclass C(\n  E, F ,\n): pass\n");
        assert_eq!(
            defs(&f),
            ["function f 2 0 1 @a@b|", "function g 5 0 1 @a @b|", "class A 6 0 0 |B", "class C 8 0 0 @d|E,F"]
        );
    }

    #[test]
    fn decorator_with_fstring_field_comment() {
        let f = facts("@route(f\"{x[\"#\"]}\"  ,  m = 1)  # c\ndef h(): pass\n");
        assert_eq!(defs(&f), ["function h 2 0 1 @route(f\"{x[\"#\"]}\",m=1)|"]);
    }

    #[test]
    fn imports_all_forms() {
        let f = facts(
            "import os, a.b as c\nfrom . import x\nfrom ..pkg.mod import (\n    y as z,\n    w,\n)\nfrom m import *\ndef f():\n    import json\nclass K:\n    from typing import Any\n",
        );
        assert_eq!(
            imports(&f),
            [
                "<module> os|||0 1",
                "<module> a.b||c|0 1",
                "<module> |x||1 2",
                "<module> pkg.mod|y|z|2 4",
                "<module> pkg.mod|w||2 5",
                "<module> m|*||0 7",
                "f json|||0 9",
                "K typing|Any||0 11",
            ]
        );
    }

    #[test]
    fn call_chains_and_scopes() {
        let f = facts(
            "import x\n@deco(arg())\ndef f(a=default(), b: ann() = 1) -> ret():\n    a.b.c(d())\n    self.m()\n    super().m()\n    super(A, self).n()\n    x[0].m()\n    f()()\n    (lambda: inner())()\n    [g(i) for i in r]\n    h(\n        1)\n    obj \\\n        .meth(\n    )\n    s = f'{fmt(1)}'\n",
        );
        let mut want = vec![
            "<module> deco 2",
            "<module> arg 2",
            "<module> default 3",
            "<module> ann 3",
            "<module> ret 3",
            "f a.b.c 4",
            "f d 4",
            "f self.m 5",
            "f super 6",
            "f super().m 6",
            "f super 7",
            "f super().n 7",
            "f ?.m 8",
            "f f 9",
            "f ? 9",
            "f ? 10",
            "f inner 10",
            "f g 11",
            "f h 12",
            // The chain's first name, not the `(` (line 15).
            "f obj.meth 14",
            "f fmt 17",
        ];
        want.sort();
        assert_eq!(calls(&f), want);
    }

    #[test]
    fn class_body_and_base_calls_scope() {
        let f = facts("class A(make()):\n    x = build()\n    def m(self, d=dflt()):\n        run()\n");
        assert_eq!(calls(&f), ["<module> make 1", "A build 2", "A dflt 3", "A.m run 4"]);
        assert_eq!(f.scope_name(MODULE_SCOPE), MODULE_SCOPE_NAME);
    }

    #[test]
    fn bom_and_crlf_lines() {
        let f = facts("\u{feff}import os\r\ndef f():\r\n    g()\r\n");
        assert_eq!(imports(&f), ["<module> os|||0 1"]);
        assert_eq!(defs(&f), ["function f 2 0 1 |"]);
        assert_eq!(calls(&f), ["f g 3"]);
    }

    #[test]
    fn skips_syntax_errors_and_non_utf8() {
        let mut lines = LineIndex::default();
        let mut out = FileFacts::default();
        let src = b"x = 1\ndef f(:\n";
        lines.reset(src);
        let e = RuffOracle.extract(Path::new("t.py"), src, &lines, &mut out).unwrap_err();
        assert_eq!(e.kind, SkipKind::Syntax);
        assert_eq!(e.line, 2);
        let src = b"x = 1\ny = '\xff'\n";
        lines.reset(src);
        let e = RuffOracle.extract(Path::new("t.py"), src, &lines, &mut out).unwrap_err();
        assert_eq!(e.kind, SkipKind::NotUtf8);
        assert_eq!(e.line, 2);
    }

    // McCabe cases ported from ruff's own tests (scores are body + 1 of the outer function,
    // equal to ruff's `get_complexity_number` over the module suite).
    #[test]
    fn mccabe_matches_ruff_test_suite() {
        assert_eq!(cc("def trivial():\n    pass\n"), 1);
        assert_eq!(cc("def e():\n    0xF00D\n"), 1);
        assert_eq!(
            cc("def f(n):\n    if n > 3:\n        return 1\n    elif n > 4:\n        return 2\n    else:\n        return 3\n"),
            3
        );
        assert_eq!(
            cc("def f():\n    if n > 3:\n        if n > 4:\n            return 1\n        else:\n            return 2\n    else:\n        return 3\n"),
            3
        );
        assert_eq!(cc("def f():\n    for i in range(10):\n        print(i)\n"), 2);
        assert_eq!(cc("def f(l):\n    for i in l:\n        print(i)\n    else:\n        print(None)\n"), 2);
        assert_eq!(cc("def f():\n    def a():\n        def b():\n            pass\n        b()\n    a()\n"), 3);
        assert_eq!(
            cc("def f():\n    try:\n        print(1)\n    except A:\n        print(2)\n    except B:\n        print(3)\n    else:\n        print(4)\n"),
            4
        );
        assert_eq!(
            cc("def f():\n    try:\n        try:\n            print(1)\n        finally:\n            print(2)\n    finally:\n        print(3)\n"),
            1
        );
        assert_eq!(
            cc("async def f(a, b, c):\n    await w(a)\n    if await b:\n        pass\n    async with c:\n        pass\n    async for x in a:\n        pass\n"),
            3
        );
        assert_eq!(cc("def f():\n    try:\n        pass\n    finally:\n        if res:\n            e()\n"), 2);
        assert_eq!(cc("def f():\n    with lock:\n        if foo:\n            print('bar')\n"), 2);
        assert_eq!(cc("def f():\n    match s:\n        case 2:\n            a()\n        case _:\n            b()\n"), 2);
        assert_eq!(
            cc("def f():\n    match s:\n        case 2:\n            a()\n        case 2:\n            b()\n        case _:\n            c()\n"),
            3
        );
        assert_eq!(cc("def f():\n    match s:\n        case 2:\n            a()\n        case x:\n            b()\n"), 2);
        assert_eq!(cc("def f():\n    match s:\n        case 2:\n            a()\n        case 5 | _:\n            b()\n"), 2);
        // Guarded catch-all is refutable.
        assert_eq!(cc("def f():\n    match s:\n        case 2:\n            a()\n        case _ if g:\n            b()\n"), 3);
        // Not counted: boolean ops, ternaries, comprehensions, assert, lambdas.
        assert_eq!(cc("def f():\n    x = a and b or c\n    y = 1 if a else 2\n    z = [i for i in r if i]\n    assert x\n    l = lambda: 1 if a else 2\n"), 1);
        // while/else and try/except*.
        assert_eq!(cc("def f():\n    while a:\n        pass\n    else:\n        pass\n"), 2);
        assert_eq!(cc("def f():\n    try:\n        pass\n    except* A:\n        pass\n    except* B:\n        pass\n"), 3);
    }

    #[test]
    fn mccabe_class_inside_function_and_nested_scores() {
        let src = "class Class:\n    def handle(self, *args, **options):\n        if args:\n            return\n\n        class ServiceProvider:\n            def a(self):\n                pass\n\n            def b(self, data):\n                if not args:\n                    pass\n\n        class Logger:\n            def c(*args, **kwargs):\n                pass\n\n            def error(self, message):\n                pass\n\n            def info(self, message):\n                pass\n\n            def exception(self):\n                pass\n\n        return ServiceProvider(Logger())\n";
        let f = facts(src);
        let got: Vec<(String, u32)> = f.defs.iter().map(|d| (s(&f, d.qualname), d.mccabe)).collect();
        // ruff's suite-level number for this module is 9 = handle's body (8) + its own base 1.
        assert_eq!(got[1], ("Class.handle".to_owned(), 9));
        assert_eq!(got[3], ("Class.handle.ServiceProvider.a".to_owned(), 1));
        assert_eq!(got[4], ("Class.handle.ServiceProvider.b".to_owned(), 2));
        assert_eq!(got[0].1, 0, "classes carry no score");
    }
}
