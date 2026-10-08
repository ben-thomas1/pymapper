//! Definitions: decorators, `def` / `async def`, `class`, PEP 695 type parameters.

use super::events::{Base, Decorator, Def, DefKind, DiagKind, Param, ParamKind, Visitor};
use super::expr::{S_ASSIGN, S_COLON, S_COMMA, Shape};
use super::parser::Parser;
use crate::lex::{Token, TokenKind as K};

impl<V: Visitor> Parser<'_, V> {
    /// Decorators followed by a `def` / `class`.
    pub(super) fn decorated(&mut self) {
        let start = self.peek(0).start;
        let mut count = 0u32;
        while self.at(K::At) {
            let at = self.bump();
            let shape = self.expr(0);
            let (names, called): (&[Token], bool) = match shape {
                Shape::Name => (&self.shape, false),
                Shape::Call => (&self.shape, true),
                _ => (&[], false),
            };
            self.v.decorator(&Decorator { at, names, called });
            count += 1;
            match self.kind(0) {
                K::Newline => {
                    self.bump();
                }
                K::EndOfFile => return,
                _ => {
                    self.diag(DiagKind::UnexpectedToken);
                    self.skip_line();
                }
            }
        }
        match (self.kind(0), self.kind(1)) {
            (K::Def, _) | (K::Async, K::Def) => self.funcdef(Some(start), count),
            (K::Class, _) => self.classdef(count, start),
            _ => {
                // Decorators must be followed by a definition.
                self.diag(DiagKind::UnexpectedToken);
            }
        }
    }

    /// `def` / `async def`. `start` is the first decorator's offset, if any.
    pub(super) fn funcdef(&mut self, start: Option<u32>, decorators: u32) {
        let first = self.peek(0);
        let is_async = self.eat(K::Async).is_some();
        let keyword = self.bump();
        let start = start.unwrap_or(first.start);
        let Some(name) = self.eat(K::Name) else {
            self.diag(DiagKind::ExpectedName);
            self.skip_line();
            return;
        };
        self.type_params.clear();
        if self.at(K::LSqb) {
            self.type_param_list();
        }
        self.params.clear();
        if self.eat(K::LPar).is_some() {
            self.param_list();
        } else {
            self.diag(DiagKind::UnexpectedToken);
        }
        let mut has_return_annotation = false;
        if self.eat(K::RArrow).is_some() {
            has_return_annotation = true;
            self.expr(S_COLON);
        }
        self.header_colon();
        let name_pos = self.pos(name.start);
        let start_line = self.pos(start).line;
        self.v.def_enter(&Def {
            kind: DefKind::Function,
            name,
            name_pos,
            keyword,
            start,
            start_line,
            is_async,
            decorators,
            type_params: &self.type_params,
            params: &self.params,
            bases: 0,
            has_return_annotation,
            depth: self.def_depth,
        });
        self.def_body(DefKind::Function);
    }

    /// Body of a definition (after its `:`), then `def_exit`.
    fn def_body(&mut self, kind: DefKind) {
        self.def_depth += 1;
        self.suite();
        self.def_depth -= 1;
        let end = self.end_here();
        self.v.def_exit(kind, end);
    }

    /// `class`. `start` is the first decorator's offset when `decorators > 0`.
    pub(super) fn classdef(&mut self, decorators: u32, start: u32) {
        let keyword = self.bump();
        let start = if decorators > 0 { start } else { keyword.start };
        let Some(name) = self.eat(K::Name) else {
            self.diag(DiagKind::ExpectedName);
            self.skip_line();
            return;
        };
        self.type_params.clear();
        if self.at(K::LSqb) {
            self.type_param_list();
        }
        let mut bases = 0u32;
        if self.eat(K::LPar).is_some() {
            bases = self.base_list();
        }
        self.header_colon();
        let name_pos = self.pos(name.start);
        let start_line = self.pos(start).line;
        self.v.def_enter(&Def {
            kind: DefKind::Class,
            name,
            name_pos,
            keyword,
            start,
            start_line,
            is_async: false,
            decorators,
            type_params: &self.type_params,
            params: &[],
            bases,
            has_return_annotation: false,
            depth: self.def_depth,
        });
        self.def_body(DefKind::Class);
    }

    /// Class bases after `(` up to and including `)`. Returns the number of `base` events.
    fn base_list(&mut self) -> u32 {
        let mut count = 0;
        loop {
            if self.eat(K::RPar).is_some() {
                return count;
            }
            let starred = matches!(self.kind(0), K::Star | K::DoubleStar);
            if starred {
                self.bump();
            }
            let mut keyword = None;
            if !starred && self.at(K::Name) && self.kind(1) == K::Equal {
                let kw = self.bump();
                self.v.keyword_arg(kw);
                self.bump();
                keyword = Some(kw);
            }
            let shape = self.expr(S_COMMA);
            if shape == Shape::Empty {
                self.diag(DiagKind::UnexpectedToken);
                self.skip_bracket_rest();
                return count;
            }
            let (names, subscripted): (&[Token], bool) = match shape {
                Shape::Name => (&self.shape, false),
                Shape::Subscript => (&self.shape, true),
                _ => (&[], false),
            };
            self.v.base(&Base { names, keyword, subscripted, starred });
            count += 1;
            if self.eat(K::Comma).is_none() && !self.at(K::RPar) {
                self.diag(DiagKind::UnexpectedToken);
                self.skip_bracket_rest();
                return count;
            }
        }
    }

    /// Parameters after `(` up to and including `)`, into `self.params`.
    fn param_list(&mut self) {
        let mut kind = ParamKind::Normal;
        loop {
            match self.kind(0) {
                K::RPar => {
                    self.bump();
                    return;
                }
                K::Slash => {
                    self.bump();
                    for p in &mut self.params {
                        p.kind = ParamKind::PositionalOnly;
                    }
                }
                K::Star => {
                    self.bump();
                    if let Some(name) = self.eat(K::Name) {
                        self.param_rest(name, ParamKind::VarPositional);
                    }
                    kind = ParamKind::KeywordOnly;
                }
                K::DoubleStar => {
                    self.bump();
                    match self.eat(K::Name) {
                        Some(name) => self.param_rest(name, ParamKind::VarKeyword),
                        None => self.diag(DiagKind::ExpectedName),
                    }
                }
                K::Name => {
                    let name = self.bump();
                    self.param_rest(name, kind);
                }
                _ => {
                    self.diag(DiagKind::UnexpectedToken);
                    self.skip_bracket_rest();
                    return;
                }
            }
            if self.eat(K::Comma).is_none() && !self.at(K::RPar) {
                self.diag(DiagKind::UnexpectedToken);
                self.skip_bracket_rest();
                return;
            }
        }
    }

    /// Annotation and default of a parameter.
    fn param_rest(&mut self, name: Token, kind: ParamKind) {
        let annotated = self.eat(K::Colon).is_some();
        if annotated {
            self.expr(S_COMMA | S_ASSIGN);
        }
        let has_default = self.eat(K::Equal).is_some();
        if has_default {
            self.expr(S_COMMA);
        }
        self.params.push(Param { name, kind, has_default, annotated });
    }

    /// PEP 695 type parameters `[T: bound = default, *Ts, **P]`, into `self.type_params`.
    pub(super) fn type_param_list(&mut self) {
        self.bump();
        loop {
            if self.eat(K::RSqb).is_some() {
                return;
            }
            if matches!(self.kind(0), K::Star | K::DoubleStar) {
                self.bump();
            }
            let Some(name) = self.eat(K::Name) else {
                self.diag(DiagKind::ExpectedName);
                self.skip_bracket_rest();
                return;
            };
            self.type_params.push(name);
            if self.eat(K::Colon).is_some() {
                self.expr(S_COMMA | S_ASSIGN);
            }
            if self.eat(K::Equal).is_some() {
                self.expr(S_COMMA);
            }
            if self.eat(K::Comma).is_none() && !self.at(K::RSqb) {
                self.diag(DiagKind::UnexpectedToken);
                self.skip_bracket_rest();
                return;
            }
        }
    }

    /// Error recovery inside an open bracket whose opener was consumed: skip
    /// past its closer (balancing nested brackets), stopping early at the end of
    /// the line or at a statement keyword (with lexer bracket recovery).
    pub(super) fn skip_bracket_rest(&mut self) {
        let mut depth = 1u32;
        loop {
            let t = self.peek(0);
            match t.kind {
                K::LPar | K::LSqb | K::LBrace => depth += 1,
                K::RPar | K::RSqb | K::RBrace => {
                    depth -= 1;
                    if depth == 0 {
                        self.bump();
                        return;
                    }
                }
                K::Newline | K::EndOfFile | K::Indent | K::Dedent => return,
                k if super::expr::is_stmt_keyword(k) => {
                    self.diag_at(DiagKind::UnclosedBracket, t);
                    if self.recover_brackets_at(t) {
                        return;
                    }
                }
                _ => {}
            }
            self.bump();
        }
    }
}
