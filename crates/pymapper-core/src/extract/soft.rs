//! Soft keywords: `match` / `case` statements and `type` aliases, and
//! parenthesised with-items. Disambiguated by lexer checkpoint rescans.

use super::events::{Block, BlockKind, DiagKind, Visitor};
use super::expr::{S_COLON, S_IF, is_augassign, is_stmt_keyword};
use super::parser::Parser;
use crate::lex::{Token, TokenKind as K};

impl<V: Visitor> Parser<'_, V> {
    /// At a statement-start `Name`: handles `match` / `type` soft-keyword
    /// statements. Returns false if the name is an ordinary identifier.
    pub(super) fn soft_keyword_stmt(&mut self) -> bool {
        let t = self.peek(0);
        match t.text(self.src) {
            b"match" if self.is_match_stmt() => {
                self.match_stmt();
                true
            }
            b"type" if self.kind(1) == K::Name && matches!(self.kind(2), K::Equal | K::LSqb) => {
                self.type_alias_stmt();
                true
            }
            _ => false,
        }
    }

    /// `match` starts a match statement iff a `:` at bracket depth 0 is
    /// directly followed by NEWLINE (a match body is always an indented block).
    fn is_match_stmt(&mut self) -> bool {
        let k1 = self.kind(1);
        if matches!(
            k1,
            K::Newline
                | K::EndOfFile
                | K::Equal
                | K::Dot
                | K::Colon
                | K::Comma
                | K::Semi
                | K::RPar
                | K::RSqb
                | K::RBrace
                | K::ColonEqual
                | K::RArrow
        ) || is_augassign(k1)
        {
            return false;
        }
        let mut first = true;
        let mut depth = 0u32;
        let mut lambdas = 0u32;
        let mut colon = false;
        self.rescan(|t| {
            if first {
                // The `match` token itself.
                first = false;
                return None;
            }
            if colon {
                return Some(t.kind == K::Newline);
            }
            match t.kind {
                K::LPar | K::LSqb | K::LBrace => depth += 1,
                K::RPar | K::RSqb | K::RBrace => {
                    if depth == 0 {
                        return Some(false);
                    }
                    depth -= 1;
                }
                K::Lambda if depth == 0 => lambdas += 1,
                K::Colon if depth == 0 => {
                    if lambdas > 0 {
                        lambdas -= 1;
                    } else {
                        colon = true;
                    }
                }
                k if is_stmt_keyword(k) => return Some(false),
                _ => {}
            }
            None
        })
    }

    fn match_stmt(&mut self) {
        let kw = self.bump();
        self.expr(S_COLON);
        self.header_colon();
        let pos = self.pos(kw.start);
        self.v.block_enter(&Block { pos, ..Block::new(BlockKind::Match, kw) });
        if self.eat(K::Newline).is_some() && self.eat(K::Indent).is_some() {
            loop {
                let t = self.peek(0);
                match t.kind {
                    K::Dedent => {
                        self.bump();
                        break;
                    }
                    K::EndOfFile => break,
                    K::Newline => {
                        self.bump();
                    }
                    K::Name if t.text(self.src) == b"case" => self.case_clause(),
                    _ => {
                        self.diag_at(DiagKind::UnexpectedToken, t);
                        self.statement_or_stray();
                    }
                }
            }
        } else {
            self.diag(DiagKind::ExpectedIndent);
            self.skip_line();
        }
        let end = self.end_here();
        self.v.block_exit(BlockKind::Match, end, false);
    }

    fn case_clause(&mut self) {
        let kw = self.bump();
        self.tgt.clear();
        self.pattern = true;
        self.expr(S_IF | S_COLON);
        self.pattern = false;
        let irrefutable = pattern_irrefutable(&self.tgt);
        let guarded = self.eat(K::If).is_some();
        if guarded {
            self.expr(S_COLON);
        }
        self.header_colon();
        let pos = self.pos(kw.start);
        self.v.block_enter(&Block { pos, irrefutable, guarded, ..Block::new(BlockKind::Case, kw) });
        self.suite();
        let next = self.peek(0);
        let last = !(next.kind == K::Name && next.text(self.src) == b"case");
        let end = self.end_here();
        self.v.block_exit(BlockKind::Case, end, last);
    }

    /// `type X[T] = value`.
    fn type_alias_stmt(&mut self) {
        self.bump();
        let name = self.bump();
        self.type_params.clear();
        if self.at(K::LSqb) {
            self.type_param_list();
        }
        let pos = self.pos(name.start);
        self.v.type_alias(name, &self.type_params, pos);
        if self.eat(K::Equal).is_none() {
            self.diag(DiagKind::UnexpectedToken);
            return;
        }
        self.expr(0);
    }

    /// At `(` after `with`: whether it opens parenthesised with-items
    /// (`with (a as b, c):`) rather than an expression (`with (a, b) as c:`):
    /// the matching `)` is followed by `:` and the parentheses directly
    /// contain `as` or `,`.
    pub(super) fn parenthesized_with_items(&mut self) -> bool {
        let mut depth = 0u32;
        let mut inner = false;
        let mut closed = false;
        self.rescan(|t| {
            if closed {
                return Some(t.kind == K::Colon && inner);
            }
            match t.kind {
                K::LPar | K::LSqb | K::LBrace => depth += 1,
                K::RPar | K::RSqb | K::RBrace => {
                    depth = depth.saturating_sub(1);
                    closed = depth == 0;
                }
                K::As | K::Comma if depth == 1 => inner = true,
                k if is_stmt_keyword(k) => return Some(false),
                _ => {}
            }
            None
        })
    }
}

/// A case pattern (its tokens) is irrefutable when it is a bare capture or
/// wildcard name, possibly parenthesised, an `|` alternative containing one,
/// or `<pattern> as NAME` whose sub-pattern is irrefutable (ruff C901 /
/// `Pattern::irrefutable_pattern`).
fn pattern_irrefutable(toks: &[Token]) -> bool {
    let mut toks = toks;
    loop {
        // Strip enclosing parentheses that match each other.
        if toks.len() >= 2 && toks[0].kind == K::LPar && toks[toks.len() - 1].kind == K::RPar && encloses(toks) {
            toks = &toks[1..toks.len() - 1];
            continue;
        }
        // `as` binds looser than `|`: `1 | x as y` is `(1 | x) as y`.
        if let Some(sub) = strip_as_target(toks) {
            toks = sub;
            continue;
        }
        break;
    }
    let mut depth = 0u32;
    let mut start = 0;
    for (i, t) in toks.iter().enumerate() {
        match t.kind {
            K::LPar | K::LSqb | K::LBrace => depth += 1,
            K::RPar | K::RSqb | K::RBrace => depth = depth.saturating_sub(1),
            K::VBar if depth == 0 => {
                if alt_irrefutable(&toks[start..i]) {
                    return true;
                }
                start = i + 1;
            }
            _ => {}
        }
    }
    alt_irrefutable(&toks[start..])
}

fn alt_irrefutable(alt: &[Token]) -> bool {
    match alt {
        [t] => t.kind == K::Name,
        [first, .., last] if first.kind == K::LPar && last.kind == K::RPar && encloses(alt) => {
            pattern_irrefutable(alt)
        }
        _ => false,
    }
}

/// `<pattern> as NAME` at depth 0 -> the sub-pattern tokens.
fn strip_as_target(toks: &[Token]) -> Option<&[Token]> {
    let n = toks.len();
    if n < 3 || toks[n - 2].kind != K::As || toks[n - 1].kind != K::Name {
        return None;
    }
    let mut depth = 0u32;
    for t in &toks[..n - 2] {
        match t.kind {
            K::LPar | K::LSqb | K::LBrace => depth += 1,
            K::RPar | K::RSqb | K::RBrace => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    if depth == 0 { Some(&toks[..n - 2]) } else { None }
}

/// `toks[0]` (an opener) is closed by the last token, not earlier.
fn encloses(toks: &[Token]) -> bool {
    let mut depth = 0u32;
    for (i, t) in toks.iter().enumerate() {
        match t.kind {
            K::LPar | K::LSqb | K::LBrace => depth += 1,
            K::RPar | K::RSqb | K::RBrace => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return i == toks.len() - 1;
                }
            }
            _ => {}
        }
    }
    false
}
