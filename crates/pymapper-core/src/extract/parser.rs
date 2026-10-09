//! Parser core: token ring buffer, line cursor, diagnostics.
//!
//! The grammar lives in `stmt.rs` (statements, blocks, definitions) and
//! `expr.rs` (expression token scanner); both extend [`Parser`].

use super::events::{DiagKind, Diagnostic, End, Param, Pos, Visitor};
use super::expr::{Level, TargetRec};
use super::lines::LineCursor;
use crate::lex::{Lexer, Suppression, Token, TokenKind};

/// Ring buffer slots (power of two). `peek(k)` supports `k < RING`.
const RING: usize = 4;
const RING_MASK: usize = RING - 1;

/// Parses `src` and drives `visitor`; returns the visitor. Suppressions are
/// dropped: see [`parse_with_suppressions`].
pub fn parse<V: Visitor>(src: &[u8], visitor: V) -> V {
    let mut p = Parser::new(src, visitor);
    p.parse_file();
    p.v
}

/// Like [`parse`], but also returns the `# pymapper: ignore[...]`
/// suppressions the lexer recorded (in source order). Use this for facts
/// extraction; [`parse`] drops them.
pub fn parse_with_suppressions<V: Visitor>(src: &[u8], visitor: V) -> (V, Vec<Suppression>) {
    let mut p = Parser::new(src, visitor);
    p.parse_file();
    p.finish()
}

/// Recursive descent parser over the pull lexer. See the module docs of
/// [`super`] for the overall design.
pub struct Parser<'src, V: Visitor> {
    pub(super) src: &'src [u8],
    pub(super) lx: Lexer<'src>,
    buf: [Token; RING],
    head: usize,
    len: usize,
    /// End offset of the last consumed significant token.
    pub(super) last_end: u32,
    pub(super) lines: LineCursor,
    pub(super) v: V,
    /// Enclosing definitions (0 = module level).
    pub(super) def_depth: u32,
    // Reusable scratch buffers (never shrink; no per-statement allocation).
    pub(super) chain: Vec<Token>,
    pub(super) names: Vec<Token>,
    pub(super) params: Vec<Param>,
    pub(super) type_params: Vec<Token>,
    /// Dotted chain of the last [`super::expr::Shape`] `Name`/`Call`/`Subscript`.
    pub(super) shape: Vec<Token>,
    /// Strings of the last [`super::expr::Shape::Strings`].
    pub(super) strings: Vec<Token>,
    /// Bracket/context stack of the expression scanner.
    pub(super) levels: Vec<Level>,
    /// Record assignment targets while scanning (see `expr.rs`).
    pub(super) collect: bool,
    pub(super) tgt: Vec<Token>,
    pub(super) tgt_recs: Vec<TargetRec>,
    pub(super) tgt_unpack: bool,
    /// An unclosed bracket was recovered from: the current statement ended
    /// early at a statement keyword (already reported).
    pub(super) resync: bool,
    /// Scanning a case pattern: no call/keyword events; tokens go to `tgt`.
    pub(super) pattern: bool,
    pub(super) pat: super::soft::PatScratch,
    pub(super) diag_count: u32,
}

impl<'src, V: Visitor> Parser<'src, V> {
    pub fn new(src: &'src [u8], v: V) -> Self {
        let lx = Lexer::new(src);
        let src = lx.source();
        let dummy = Token { start: 0, len: 0, kind: TokenKind::EndOfFile };
        Parser {
            src,
            lx,
            buf: [dummy; RING],
            head: 0,
            len: 0,
            last_end: 0,
            lines: LineCursor::new(src),
            v,
            def_depth: 0,
            chain: Vec::new(),
            names: Vec::new(),
            params: Vec::new(),
            type_params: Vec::new(),
            shape: Vec::new(),
            strings: Vec::new(),
            levels: Vec::new(),
            collect: false,
            tgt: Vec::new(),
            tgt_recs: Vec::new(),
            tgt_unpack: false,
            pattern: false,
            pat: super::soft::PatScratch::default(),
            resync: false,
            diag_count: 0,
        }
    }

    /// The visitor.
    pub fn visitor(&mut self) -> &mut V {
        &mut self.v
    }

    pub fn into_visitor(self) -> V {
        self.v
    }

    /// The visitor and the `# pymapper: ignore[...]` suppressions the lexer
    /// recorded (call after [`Parser::parse_file`]).
    pub fn finish(mut self) -> (V, Vec<Suppression>) {
        let sup = self.lx.take_suppressions();
        (self.v, sup)
    }

    /// Diagnostics reported so far.
    pub fn diagnostics(&self) -> u32 {
        self.diag_count
    }

    /// Pulls the next token from the lexer, reporting and skipping error tokens.
    #[inline]
    fn lex(&mut self) -> Token {
        loop {
            let open = self.lx.bracket_depth();
            let t = self.lx.next_token();
            if !t.kind.is_error() {
                // Inside brackets the lexer joins lines, so a NEWLINE with a
                // bracket open means the input ended first (the lexer closes
                // them all there). Reported once, at end of input.
                if open > 0 && t.kind == TokenKind::Newline {
                    self.diag_at(DiagKind::UnclosedBracket, t);
                }
                return t;
            }
            self.diag_at(DiagKind::Lex, t);
        }
    }

    /// The `k`-th token ahead (`k < 4`), without consuming.
    #[inline(always)]
    pub(super) fn peek(&mut self, k: usize) -> Token {
        debug_assert!(k < RING);
        if self.len <= k {
            self.fill(k);
        }
        self.buf[(self.head + k) & RING_MASK]
    }

    /// Lexes until `k + 1` tokens are buffered.
    #[inline]
    fn fill(&mut self, k: usize) {
        while self.len <= k {
            let t = self.lex();
            self.buf[(self.head + self.len) & RING_MASK] = t;
            self.len += 1;
        }
    }

    #[inline(always)]
    pub(super) fn kind(&mut self, k: usize) -> TokenKind {
        self.peek(k).kind
    }

    #[inline]
    pub(super) fn at(&mut self, kind: TokenKind) -> bool {
        self.kind(0) == kind
    }

    /// Consumes the current token. EndOfFile is never consumed (it repeats).
    #[inline(always)]
    pub(super) fn bump(&mut self) -> Token {
        let t = self.peek(0);
        use TokenKind as K;
        match t.kind {
            K::EndOfFile => {}
            K::Newline | K::Indent | K::Dedent => {
                self.head = (self.head + 1) & RING_MASK;
                self.len -= 1;
            }
            _ => {
                self.head = (self.head + 1) & RING_MASK;
                self.len -= 1;
                self.last_end = t.end();
                self.v.token(t);
            }
        }
        t
    }

    /// Consumes the current token if it has `kind`.
    #[inline]
    pub(super) fn eat(&mut self, kind: TokenKind) -> Option<Token> {
        if self.at(kind) { Some(self.bump()) } else { None }
    }

    /// Unbounded lookahead within the current logical line: feeds `f` the
    /// tokens from the current one onwards (buffered tokens first, then fresh
    /// ones from the lexer) until it returns a decision, then rewinds the
    /// lexer to a checkpoint so nothing is consumed. Stops with `false` at the
    /// end of the logical line (NEWLINE is offered to `f` first) or at
    /// INDENT/DEDENT/EOF. Error tokens are skipped silently (they are reported
    /// when actually consumed).
    pub(super) fn rescan(&mut self, mut f: impl FnMut(Token) -> Option<bool>) -> bool {
        use TokenKind as K;
        for i in 0..self.len {
            let t = self.buf[(self.head + i) & RING_MASK];
            if let Some(r) = f(t) {
                return r;
            }
            if matches!(t.kind, K::Newline | K::EndOfFile | K::Indent | K::Dedent) {
                return false;
            }
        }
        // The checkpoint is the state right after the last buffered token. The
        // scan never passes a NEWLINE, so the indent stack is untouched.
        let cp = self.lx.checkpoint();
        let r = loop {
            let t = self.lx.next_token();
            if t.kind.is_error() {
                continue;
            }
            if let Some(r) = f(t) {
                break r;
            }
            if matches!(t.kind, K::Newline | K::EndOfFile | K::Indent | K::Dedent) {
                break false;
            }
        };
        self.lx.restore(cp);
        r
    }

    /// Unclosed-bracket recovery at statement keyword `tok`: reports it, drops
    /// the buffered lookahead and the lexer's open brackets and resumes lexing
    /// at `tok`, then flags the current statement as ended (`resync`). Returns
    /// false (nothing changed) when the lexer cannot recover there (inside an
    /// f-string) or no bracket is open.
    pub(super) fn recover_unclosed(&mut self, tok: Token) -> bool {
        if self.lx.bracket_depth() == 0 || !self.lx.recover_at(tok.start) {
            return false;
        }
        self.diag_at(DiagKind::UnclosedBracket, tok);
        self.len = 0;
        self.resync = true;
        true
    }

    /// Clears `resync`, returning whether it was set.
    #[inline]
    pub(super) fn take_resync(&mut self) -> bool {
        std::mem::take(&mut self.resync)
    }

    /// Position of byte offset `off`.
    #[inline]
    pub(super) fn pos(&mut self, off: u32) -> Pos {
        self.lines.pos(off)
    }

    /// End marker at the last consumed token.
    #[inline]
    pub(super) fn end_here(&mut self) -> End {
        let end = self.last_end;
        // The last token may span lines (triple-quoted string): use its end.
        let line = self.pos(end.saturating_sub(1)).line;
        End { end, line }
    }

    pub(super) fn diag_at(&mut self, kind: DiagKind, token: Token) {
        self.diag_count += 1;
        let pos = self.pos(token.start);
        self.v.diagnostic(&Diagnostic { kind, token, pos });
    }

    /// Reports `kind` at the current token.
    pub(super) fn diag(&mut self, kind: DiagKind) {
        let t = self.peek(0);
        self.diag_at(kind, t);
    }

    /// Skips to the end of the logical line (consuming its NEWLINE).
    pub(super) fn skip_line(&mut self) {
        use TokenKind as K;
        loop {
            match self.kind(0) {
                K::EndOfFile => return,
                K::Newline => {
                    self.bump();
                    return;
                }
                // INDENT/DEDENT only appear after a NEWLINE; stop so blocks stay balanced.
                K::Indent | K::Dedent => return,
                _ => {
                    self.bump();
                }
            }
        }
    }
}
