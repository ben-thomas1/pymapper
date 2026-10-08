//! Expression token scanner.

use super::events::{DiagKind, Visitor};
use super::parser::Parser;
use crate::lex::{Token, TokenKind as K};

/// Stop conditions for [`Parser::expr`], checked at bracket depth 0 (relative
/// to where the scan started). NEWLINE, EOF, `;`, INDENT/DEDENT, an unmatched
/// closing bracket and statement-only keywords always stop.
pub(super) type Stop = u8;
/// `:` (compound header, annotation, slice/dict owner). Lambda colons are not stops.
pub(super) const S_COLON: Stop = 1;
/// `=` and augmented assignment operators.
pub(super) const S_ASSIGN: Stop = 2;
/// `,`.
pub(super) const S_COMMA: Stop = 4;
/// `as`.
pub(super) const S_AS: Stop = 8;
/// `in` (for-loop targets).
pub(super) const S_IN: Stop = 16;
/// `if` (case guard).
pub(super) const S_IF: Stop = 32;

/// Shape of a scanned expression (see [`Parser::expr`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Shape {
    /// No tokens.
    Empty,
    /// A plain dotted name; chain in `Parser::shape`.
    Name,
    /// A dotted name followed by one call; chain in `Parser::shape`.
    Call,
    /// A dotted name followed by one subscript; chain in `Parser::shape`.
    Subscript,
    /// A list/tuple/set display of plain strings; strings in `Parser::strings`.
    Strings,
    Other,
}

/// Statement-only hard keywords: never part of an expression.
#[inline]
pub(super) fn is_stmt_keyword(k: K) -> bool {
    matches!(
        k,
        K::Def
            | K::Class
            | K::Return
            | K::Import
            | K::While
            | K::Try
            | K::Except
            | K::Finally
            | K::With
            | K::Pass
            | K::Break
            | K::Continue
            | K::Raise
            | K::Global
            | K::Nonlocal
            | K::Del
            | K::Assert
            | K::Elif
    )
}

#[inline]
pub(super) fn is_augassign(k: K) -> bool {
    (k as u8) >= (K::PlusEqual as u8) && (k as u8) <= (K::DoubleSlashEqual as u8) && k != K::DoubleSlash
        || k == K::AtEqual
}

#[inline]
fn is_open(k: K) -> bool {
    matches!(k, K::LPar | K::LSqb | K::LBrace)
}

#[inline]
fn is_close(k: K) -> bool {
    matches!(k, K::RPar | K::RSqb | K::RBrace)
}

impl<V: Visitor> Parser<'_, V> {
    /// Scans one expression (list) up to a stop token at depth 0, which is left
    /// unconsumed. Emits expression events. Returns the expression's shape.
    pub(super) fn expr(&mut self, stop: Stop) -> Shape {
        let mut depth = 0u32;
        let mut n = 0u32;
        loop {
            let t = self.peek(0);
            match t.kind {
                K::Newline | K::EndOfFile | K::Indent | K::Dedent => break,
                k if is_stmt_keyword(k) => {
                    if depth > 0 {
                        self.diag_at(DiagKind::UnclosedBracket, t);
                        if self.recover_brackets_at(t) {
                            break;
                        }
                        self.bump();
                        continue;
                    }
                    break;
                }
                _ if depth == 0 && self.stops(t, stop) => break,
                k if is_open(k) => depth += 1,
                k if is_close(k) => {
                    if depth == 0 {
                        break;
                    }
                    depth -= 1;
                }
                _ => {}
            }
            self.bump();
            n += 1;
        }
        if n == 0 { Shape::Empty } else { Shape::Other }
    }

    #[inline]
    fn stops(&mut self, t: Token, stop: Stop) -> bool {
        match t.kind {
            K::Semi => true,
            K::Colon => stop & S_COLON != 0,
            K::Equal => stop & S_ASSIGN != 0,
            K::Comma => stop & S_COMMA != 0,
            K::As => stop & S_AS != 0,
            K::In => stop & S_IN != 0,
            K::If => stop & S_IF != 0,
            k if is_augassign(k) => stop & S_ASSIGN != 0,
            _ => false,
        }
    }

    /// Assignment targets (for-loop / `as`) up to a stop token. `op` is the
    /// `for` / `as` keyword. Placeholder until target analysis lands.
    pub(super) fn targets(&mut self, _op: Token, stop: Stop) {
        self.expr(stop);
    }

    /// Type expression of an `except` clause; last names of dotted names into `self.names`.
    pub(super) fn except_type(&mut self) {
        self.expr(S_AS | S_COLON | S_COMMA);
    }

    /// At `(` after `with`: whether it opens parenthesised with-items
    /// (`with (a as b, c):`) rather than an expression (`with (a, b) as c:`).
    pub(super) fn parenthesized_with_items(&mut self) -> bool {
        false
    }
}
