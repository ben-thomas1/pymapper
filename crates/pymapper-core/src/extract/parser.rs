//! Parser core: token ring buffer, line cursor, diagnostics.
//!
//! The grammar lives in `stmt.rs` (statements, blocks, definitions) and
//! `expr.rs` (expression token scanner); both extend [`Parser`].

use super::events::{DiagKind, Diagnostic, End, Param, Pos, Visitor};
use super::expr::{Level, TargetRec};
use crate::lex::{Lexer, Token, TokenKind};

/// Ring buffer slots (power of two). `peek(k)` supports `k < RING`.
const RING: usize = 4;
const RING_MASK: usize = RING - 1;

/// Parses `src` and drives `visitor`; returns the visitor.
pub fn parse<V: Visitor>(src: &[u8], visitor: V) -> V {
    let mut p = Parser::new(src, visitor);
    p.parse_file();
    p.v
}

/// Forward line counter. Offsets are usually queried in increasing order, so
/// each byte is scanned about once; backwards queries rescan from the start.
pub(super) struct LineCursor {
    off: u32,
    line: u32,
    line_start: u32,
}

impl LineCursor {
    #[inline]
    pub(super) fn pos(&mut self, src: &[u8], off: u32) -> Pos {
        if off < self.off {
            self.off = 0;
            self.line = 1;
            self.line_start = 0;
        }
        let mut i = self.off as usize;
        let end = (off as usize).min(src.len());
        while i < end {
            let c = src[i];
            i += 1;
            // `\n`, `\r\n` and lone `\r` each end a line.
            if c == b'\n' || (c == b'\r' && src.get(i) != Some(&b'\n')) {
                self.line += 1;
                self.line_start = i as u32;
            }
        }
        self.off = end as u32;
        Pos { line: self.line, col: off - self.line_start.min(off) }
    }
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
            lines: LineCursor { off: 0, line: 1, line_start: 0 },
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

    /// Diagnostics reported so far.
    pub fn diagnostics(&self) -> u32 {
        self.diag_count
    }

    /// Pulls the next token from the lexer, reporting and skipping error tokens.
    #[inline]
    fn lex(&mut self) -> Token {
        loop {
            let t = self.lx.next_token();
            if !t.kind.is_error() {
                return t;
            }
            self.diag_at(DiagKind::Lex, t);
        }
    }

    /// The `k`-th token ahead (`k < 4`), without consuming.
    #[inline]
    pub(super) fn peek(&mut self, k: usize) -> Token {
        debug_assert!(k < RING);
        while self.len <= k {
            let t = self.lex();
            self.buf[(self.head + self.len) & RING_MASK] = t;
            self.len += 1;
        }
        self.buf[(self.head + k) & RING_MASK]
    }

    #[inline]
    pub(super) fn kind(&mut self, k: usize) -> TokenKind {
        self.peek(k).kind
    }

    #[inline]
    pub(super) fn at(&mut self, kind: TokenKind) -> bool {
        self.kind(0) == kind
    }

    /// Consumes the current token. EndOfFile is never consumed (it repeats).
    #[inline]
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

    /// Drops the buffered lookahead and the lexer's open brackets, resuming at
    /// `tok` (error recovery for an unclosed bracket). Returns false when the
    /// lexer cannot recover there (inside an f-string).
    pub(super) fn recover_brackets_at(&mut self, tok: Token) -> bool {
        if !self.lx.recover_at(tok.start) {
            return false;
        }
        self.len = 0;
        true
    }

    /// Position of byte offset `off`.
    #[inline]
    pub(super) fn pos(&mut self, off: u32) -> Pos {
        self.lines.pos(self.src, off)
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
