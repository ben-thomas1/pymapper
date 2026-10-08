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
    #[allow(dead_code)]
    fn chain(&self, ts: &[Token]) -> String {
        ts.iter().map(|&t| self.t(t)).collect::<Vec<_>>().join(".")
    }
}

impl Visitor for Rec<'_> {
    fn token(&mut self, _tok: Token) {
        self.tokens += 1;
    }
    fn diagnostic(&mut self, d: &Diagnostic) {
        let t = self.t(d.token);
        self.out.push(format!("diag {:?} {}:{} {t}", d.kind, d.pos.line, d.pos.col));
    }
}

fn run(src: &str) -> Rec<'_> {
    parse(src.as_bytes(), Rec { src: src.as_bytes(), out: Vec::new(), tokens: 0 })
}

#[allow(dead_code)]
fn events(src: &str) -> Vec<String> {
    run(src).out
}

#[test]
fn smoke_consumes_every_token() {
    let r = run("x = 1\nif x:\n    y = (1,\n 2)\n");
    assert_eq!(r.tokens, 13);
    assert!(r.out.is_empty(), "{:?}", r.out);
}

#[test]
fn noop_visitor_and_empty_inputs() {
    for src in ["", "\n", "   \n\n", "\u{feff}", "#c\n", "x"] {
        parse(src.as_bytes(), NoopVisitor);
    }
}
