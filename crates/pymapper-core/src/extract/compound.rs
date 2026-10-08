//! Compound statements: if/while/for/try/with (match is in `soft.rs`).
//!
//! Header expression events are emitted before the clause's `block_enter`
//! (the header is evaluated outside the clause body).

use super::events::{Block, BlockKind, DiagKind, IfCond, Visitor};
use super::expr::{S_AS, S_COLON, S_COMMA, S_IN, Shape};
use super::parser::Parser;
use crate::lex::{Token, TokenKind as K};

impl Block<'static> {
    pub(super) const fn new(kind: BlockKind, keyword: Token) -> Self {
        Block {
            kind,
            keyword,
            pos: super::events::Pos { line: 0, col: 0 },
            is_async: false,
            cond: IfCond::Other,
            star: false,
            except_names: &[],
            irrefutable: false,
            guarded: false,
        }
    }
}

impl<V: Visitor> Parser<'_, V> {
    /// `if`/`while`/`for`/`try`/`with`, optionally after `async`.
    pub(super) fn compound(&mut self) {
        let is_async = self.eat(K::Async).is_some();
        match self.kind(0) {
            K::If => self.if_stmt(),
            K::While => self.while_stmt(),
            K::For => self.for_stmt(is_async),
            K::Try => self.try_stmt(),
            K::With => self.with_stmt(is_async),
            _ => unreachable!("compound() called on a non-compound token"),
        }
    }

    /// Emits `block_enter`, parses the suite, emits `block_exit`.
    /// `Except` blocks take their names from `self.names`.
    pub(super) fn clause(&mut self, b: Block<'static>) {
        self.clause_ex(b, false);
    }

    pub(super) fn clause_ex(&mut self, b: Block<'static>, last_case: bool) {
        let pos = self.pos(b.keyword.start);
        let names: &[Token] = if b.kind == BlockKind::Except { &self.names } else { &[] };
        self.v.block_enter(&Block { pos, except_names: names, ..b });
        self.suite();
        let end = self.end_here();
        self.v.block_exit(b.kind, end, last_case);
    }

    /// `if`/`elif` condition up to (not including) the `:`.
    fn if_condition(&mut self) -> IfCond {
        // `__name__ == "__main__"` in either order.
        let (a, b, c, d) = (self.peek(0), self.peek(1), self.peek(2), self.peek(3));
        if b.kind == K::EqEqual && d.kind == K::Colon {
            let src = self.src;
            let is_name = |t: Token| t.kind == K::Name && t.text(src) == b"__name__";
            let is_main = |t: Token| {
                t.kind == K::String && matches!(t.text(src), b"'__main__'" | b"\"__main__\"")
            };
            if (is_name(a) && is_main(c)) || (is_main(a) && is_name(c)) {
                self.expr(S_COLON);
                return IfCond::MainGuard;
            }
        }
        let negated = a.kind == K::Not && b.kind == K::Name;
        if negated {
            self.bump();
        }
        let shape = self.expr(S_COLON);
        let tc = shape == Shape::Name && self.shape.last().is_some_and(|t| t.text(self.src) == b"TYPE_CHECKING");
        match (tc, negated) {
            (true, false) => IfCond::TypeChecking,
            (true, true) => IfCond::NotTypeChecking,
            _ => IfCond::Other,
        }
    }

    fn if_stmt(&mut self) {
        let kw = self.bump();
        let cond = self.if_condition();
        self.header_colon();
        self.clause(Block { cond, ..Block::new(BlockKind::If, kw) });
        loop {
            match self.kind(0) {
                K::Elif => {
                    let kw = self.bump();
                    let cond = self.if_condition();
                    self.header_colon();
                    self.clause(Block { cond, ..Block::new(BlockKind::Elif, kw) });
                }
                K::Else => {
                    let kw = self.bump();
                    self.header_colon();
                    self.clause(Block::new(BlockKind::IfElse, kw));
                    return;
                }
                _ => return,
            }
        }
    }

    /// Optional `else:` of a loop.
    fn loop_else(&mut self) {
        if let Some(kw) = self.eat(K::Else) {
            self.header_colon();
            self.clause(Block::new(BlockKind::LoopElse, kw));
        }
    }

    fn while_stmt(&mut self) {
        let kw = self.bump();
        self.expr(S_COLON);
        self.header_colon();
        self.clause(Block::new(BlockKind::While, kw));
        self.loop_else();
    }

    fn for_stmt(&mut self, is_async: bool) {
        let kw = self.bump();
        self.targets(kw, S_IN | S_COLON);
        if self.eat(K::In).is_none() {
            self.diag(DiagKind::UnexpectedToken);
        }
        self.expr(S_COLON);
        self.header_colon();
        self.clause(Block { is_async, ..Block::new(BlockKind::For, kw) });
        self.loop_else();
    }

    fn try_stmt(&mut self) {
        let kw = self.bump();
        self.header_colon();
        self.clause(Block::new(BlockKind::Try, kw));
        while self.at(K::Except) {
            let kw = self.bump();
            let star = self.eat(K::Star).is_some();
            self.names.clear();
            if !self.at(K::Colon) {
                // PEP 758 (3.14): `except A, B:` is a tuple of types. (The
                // identical Python 2 `except E, name:` is read the same way.)
                self.except_type();
                if let Some(as_kw) = self.eat(K::As) {
                    self.targets(as_kw, S_COLON);
                }
            }
            self.header_colon();
            self.clause(Block { star, ..Block::new(BlockKind::Except, kw) });
        }
        if let Some(kw) = self.eat(K::Else) {
            self.header_colon();
            self.clause(Block::new(BlockKind::TryElse, kw));
        }
        if let Some(kw) = self.eat(K::Finally) {
            self.header_colon();
            self.clause(Block::new(BlockKind::Finally, kw));
        }
    }

    fn with_stmt(&mut self, is_async: bool) {
        let kw = self.bump();
        if self.at(K::LPar) && self.parenthesized_with_items() {
            self.bump();
            self.with_items(S_COMMA | S_AS);
            if self.eat(K::RPar).is_none() {
                self.diag(DiagKind::UnexpectedToken);
            }
        } else {
            self.with_items(S_COMMA | S_AS | S_COLON);
        }
        self.header_colon();
        self.clause(Block { is_async, ..Block::new(BlockKind::With, kw) });
    }

    /// `item [as target], ...` up to a token that is not `,`.
    fn with_items(&mut self, stop: u8) {
        loop {
            self.expr(stop);
            if let Some(as_kw) = self.eat(K::As) {
                self.targets(as_kw, stop & !S_AS);
            }
            if self.eat(K::Comma).is_none() {
                return;
            }
        }
    }
}
