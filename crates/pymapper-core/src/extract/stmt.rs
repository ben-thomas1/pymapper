//! Statements, blocks and definitions.

use super::events::{DeclKind, DiagKind, Import, Visitor};
use super::parser::Parser;
use crate::lex::{Token, TokenKind as K};

impl<V: Visitor> Parser<'_, V> {
    /// Parses the whole file.
    pub fn parse_file(&mut self) {
        loop {
            match self.kind(0) {
                K::EndOfFile => return,
                // A stray DEDENT can only follow a recovered INDENT error.
                K::Dedent => {
                    self.bump();
                }
                _ => self.statement_or_stray(),
            }
        }
    }

    /// One statement, or a stray NEWLINE / unexpected INDENT block.
    fn statement_or_stray(&mut self) {
        match self.kind(0) {
            K::Newline => {
                self.bump();
            }
            K::Indent => {
                self.diag(DiagKind::UnexpectedIndent);
                self.bump();
                self.block_body();
            }
            _ => self.statement(),
        }
    }

    /// Statements after an INDENT up to and including the matching DEDENT.
    /// Recursion depth is bounded by the lexer's fixed indent stack.
    pub(super) fn block_body(&mut self) {
        loop {
            match self.kind(0) {
                K::Dedent => {
                    self.bump();
                    return;
                }
                K::EndOfFile => return,
                _ => self.statement_or_stray(),
            }
        }
    }

    /// The body after a compound header's `:` (already consumed).
    pub(super) fn suite(&mut self) {
        match self.kind(0) {
            K::Newline => {
                self.bump();
                if self.eat(K::Indent).is_some() {
                    self.block_body();
                } else {
                    self.diag(DiagKind::ExpectedIndent);
                }
            }
            K::EndOfFile | K::Indent | K::Dedent => self.diag(DiagKind::ExpectedIndent),
            _ => self.simple_stmts(),
        }
    }

    /// Expects the `:` ending a compound header; on error reports it and skips
    /// the rest of the line (the body, if indented, is still parsed).
    pub(super) fn header_colon(&mut self) {
        if self.eat(K::Colon).is_none() {
            self.diag(DiagKind::ExpectedColon);
            self.skip_to_colon_or_eol();
        }
    }

    /// Error recovery inside a header: skip to a `:` (consumed) or the end of the line.
    pub(super) fn skip_to_colon_or_eol(&mut self) {
        loop {
            match self.kind(0) {
                K::Colon => {
                    self.bump();
                    return;
                }
                K::Newline | K::EndOfFile | K::Indent | K::Dedent => return,
                _ => {
                    self.bump();
                }
            }
        }
    }

    fn statement(&mut self) {
        match self.kind(0) {
            K::At => self.decorated(),
            K::Def => self.funcdef(None, 0),
            K::Class => self.classdef(0, 0),
            K::Async => match self.kind(1) {
                K::Def => self.funcdef(None, 0),
                K::For | K::With => self.compound(),
                _ => self.simple_stmts(),
            },
            K::If | K::While | K::For | K::Try | K::With => self.compound(),
            K::Else | K::Elif | K::Except | K::Finally => {
                // Clause without its statement: report, then parse the body so
                // indentation stays balanced.
                self.diag(DiagKind::UnexpectedToken);
                self.bump();
                self.skip_to_colon_or_eol();
                self.suite();
            }
            _ => self.simple_stmts(),
        }
    }

    /// Simple statements separated by `;` up to the NEWLINE (consumed).
    pub(super) fn simple_stmts(&mut self) {
        loop {
            self.simple_stmt();
            if self.eat(K::Semi).is_some() {
                if matches!(self.kind(0), K::Newline | K::EndOfFile) {
                    break;
                }
                continue;
            }
            break;
        }
        match self.kind(0) {
            K::Newline => {
                self.bump();
            }
            K::EndOfFile | K::Dedent | K::Indent => {}
            _ => {
                self.diag(DiagKind::UnexpectedToken);
                self.skip_line();
            }
        }
    }

    fn simple_stmt(&mut self) {
        match self.kind(0) {
            K::Pass | K::Break | K::Continue => {
                self.bump();
            }
            K::Return | K::Raise | K::Del | K::Assert => {
                self.bump();
                self.expr(0);
            }
            K::Global | K::Nonlocal => self.decl_stmt(),
            K::Import => self.import_stmt(),
            K::From => self.import_from_stmt(),
            _ => self.expr_stmt(),
        }
    }

    fn decl_stmt(&mut self) {
        let kind = if self.bump().kind == K::Global { DeclKind::Global } else { DeclKind::Nonlocal };
        self.names.clear();
        loop {
            match self.eat(K::Name) {
                Some(t) => self.names.push(t),
                None => {
                    self.diag(DiagKind::ExpectedName);
                    break;
                }
            }
            if self.eat(K::Comma).is_none() {
                break;
            }
        }
        self.v.decl(kind, &self.names);
    }

    /// Dotted name into `self.names` (cleared first). False if no name.
    fn dotted_name(&mut self) -> bool {
        self.names.clear();
        let Some(t) = self.eat(K::Name) else {
            self.diag(DiagKind::ExpectedName);
            return false;
        };
        self.names.push(t);
        while self.at(K::Dot) && self.kind(1) == K::Name {
            self.bump();
            let t = self.bump();
            self.names.push(t);
        }
        true
    }

    /// `as NAME` alias, if present.
    fn alias(&mut self) -> Option<Token> {
        self.eat(K::As)?;
        let n = self.eat(K::Name);
        if n.is_none() {
            self.diag(DiagKind::ExpectedName);
        }
        n
    }

    fn import_stmt(&mut self) {
        let keyword = self.bump();
        loop {
            if !self.dotted_name() {
                return;
            }
            let alias = self.alias();
            let pos = self.pos(self.names[0].start);
            self.v.import(&Import {
                module: &self.names,
                name: None,
                alias,
                level: 0,
                star: false,
                from: false,
                keyword,
                pos,
            });
            if self.eat(K::Comma).is_none() {
                return;
            }
        }
    }

    fn import_from_stmt(&mut self) {
        let keyword = self.bump();
        let mut level = 0u32;
        loop {
            match self.kind(0) {
                K::Dot => level += 1,
                K::Ellipsis => level += 3,
                _ => break,
            }
            self.bump();
        }
        if self.at(K::Name) {
            self.dotted_name();
        } else {
            self.names.clear();
            if level == 0 {
                self.diag(DiagKind::ExpectedName);
            }
        }
        if self.eat(K::Import).is_none() {
            self.diag(DiagKind::UnexpectedToken);
            return;
        }
        if let Some(star) = self.eat(K::Star) {
            let pos = self.pos(star.start);
            self.v.import(&Import {
                module: &self.names,
                name: None,
                alias: None,
                level,
                star: true,
                from: true,
                keyword,
                pos,
            });
            return;
        }
        let paren = self.eat(K::LPar).is_some();
        loop {
            if paren && self.eat(K::RPar).is_some() {
                return;
            }
            let Some(name) = self.eat(K::Name) else {
                self.diag(DiagKind::ExpectedName);
                if paren {
                    self.skip_past_rpar();
                }
                return;
            };
            let alias = self.alias();
            let pos = self.pos(name.start);
            self.v.import(&Import {
                module: &self.names,
                name: Some(name),
                alias,
                level,
                star: false,
                from: true,
                keyword,
                pos,
            });
            if self.eat(K::Comma).is_none() {
                if paren && self.eat(K::RPar).is_none() {
                    self.diag(DiagKind::UnexpectedToken);
                    self.skip_past_rpar();
                }
                return;
            }
        }
    }

    /// Recovery inside parentheses: skip to and past `)`, or to the end of the line.
    fn skip_past_rpar(&mut self) {
        loop {
            match self.kind(0) {
                K::RPar => {
                    self.bump();
                    return;
                }
                K::Newline | K::EndOfFile | K::Indent | K::Dedent => return,
                k if super::expr::is_stmt_keyword(k) => return,
                _ => {
                    self.bump();
                }
            }
        }
    }
}
