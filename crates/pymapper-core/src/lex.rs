//! Pull lexer over `&[u8]` (design: Front end).
//!
//! Full Python 3.14 tokenisation without requiring UTF-8: INDENT/DEDENT,
//! NEWLINE vs NL, implicit bracket joining, backslash continuation, BOM, all
//! string prefixes including t-strings, triple quotes, PEP 701 nested f/t-strings
//! (inline mode stack), all numeric forms, longest-match operators. Soft keywords
//! are lexed as names. `# pymapper: ignore[...]` comments are recorded as
//! [`Suppression`]s.
//!
//! The lexer never panics and never stops early on arbitrary bytes: invalid
//! input produces `Error*` tokens and lexing continues. Once input is exhausted
//! it yields a final NEWLINE (if the last line has tokens), the outstanding
//! DEDENTs, then [`TokenKind::EndOfFile`] forever.
//!
//! No allocation happens while lexing except for recording suppressions. All
//! state lives in fixed arrays, so [`Lexer::checkpoint`]/[`Lexer::restore`] are
//! plain copies.
//!
//! Comments and NL tokens are trivia the parser does not need; they are skipped
//! unless trivia mode is enabled ([`Lexer::with_trivia`]). Suppressions are
//! recorded in both modes.

mod scan;
mod suppress;
mod token;

#[cfg(test)]
mod tests;

pub use suppress::Suppression;
pub use token::{Token, TokenKind};

use scan::{IDENT, find2, find4, skip_ident, skip_spaces};

/// Longest source the lexer accepts: offsets are `u32`. Longer inputs are
/// truncated to this length (see [`Lexer::truncated`]); callers should skip such
/// files with a diagnostic.
pub const MAX_SOURCE_LEN: usize = u32::MAX as usize;

/// Indentation levels kept on the fixed indent stack (CPython allows 100).
pub const MAX_INDENT: usize = 128;

/// Frames on the fixed f/t-string mode stack. Each nested f-string uses one
/// frame for the string and one per open replacement field, so this allows
/// about 15 levels of nesting; deeper input yields [`TokenKind::ErrorFString`].
pub const MAX_MODES: usize = 32;

#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
struct Level {
    /// Column with tabs to multiples of 8.
    col: u32,
    /// Column with tabs counted as 1 (for Python's TabError consistency check).
    alt: u32,
    /// Recovery level pushed for a dedent to an unknown column (no INDENT was emitted
    /// for it, so popping it emits no DEDENT).
    phantom: bool,
}

/// One entry of the f/t-string mode stack.
///
/// A string frame (`F_FIELD` clear) is in literal mode. A field frame is an open
/// replacement field in expression mode, or in format-spec mode when `F_SPEC` is
/// set. Field frames copy the string bits of their owning string.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
struct Frame {
    /// String frame: bracket depth when the string started.
    /// Field frame: bracket depth before its `{`.
    depth: u32,
    flags: u8,
}

const F_FIELD: u8 = 1;
const F_SPEC: u8 = 2;
const F_TRIPLE: u8 = 4;
const F_DQUOTE: u8 = 8;
const F_RAW: u8 = 16;
const F_TSTR: u8 = 32;
const STR_BITS: u8 = F_TRIPLE | F_DQUOTE | F_RAW | F_TSTR;

impl Frame {
    #[inline]
    fn quote(self) -> u8 {
        if self.flags & F_DQUOTE != 0 { b'"' } else { b'\'' }
    }

    #[inline]
    fn triple(self) -> bool {
        self.flags & F_TRIPLE != 0
    }

    #[inline]
    fn middle_kind(self) -> TokenKind {
        if self.flags & F_TSTR != 0 { TokenKind::TStringMiddle } else { TokenKind::FStringMiddle }
    }
}

// Bracket kinds on the 2-bit bitstack. 0 = unknown (pushed out past 32 levels).
const B_PAREN: u64 = 1;
const B_SQB: u64 = 2;
const B_BRACE: u64 = 3;

// String prefix flags.
const P_RAW: u8 = 1;
const P_F: u8 = 2;
const P_T: u8 = 4;

/// Everything that changes while lexing, except the indent stack contents
/// (invariant within a logical line) and recorded suppressions.
#[derive(Clone, Copy)]
struct State {
    pos: u32,
    /// Open bracket count (all kinds, including f-string replacement-field braces).
    depth: u32,
    /// Kinds of the innermost 32 open brackets, 2 bits each, innermost in the low bits.
    kinds: u64,
    pending_dedents: u32,
    indent_len: u32,
    modes_len: u32,
    pending_error: Option<TokenKind>,
    /// Next token starts a physical line whose indentation is not yet processed.
    at_bol: bool,
    /// The current logical line is blank (only whitespace/comment).
    blank: bool,
    /// EndOfFile has been yielded through the `Iterator` impl.
    finished: bool,
    modes: [Frame; MAX_MODES],
}

/// Saved lexer position; see [`Lexer::checkpoint`].
#[derive(Clone, Copy)]
pub struct Checkpoint {
    st: State,
    suppressions: u32,
    indent_gen: u32,
}

/// Pull lexer. Call [`Lexer::next_token`] (or iterate) until [`TokenKind::EndOfFile`].
pub struct Lexer<'src> {
    src: &'src [u8],
    st: State,
    /// Bumped on every indent-stack mutation; lets `restore` detect misuse.
    indent_gen: u32,
    trivia: bool,
    truncated: bool,
    suppressions: Vec<Suppression>,
    indent: [Level; MAX_INDENT],
}

impl<'src> Lexer<'src> {
    /// Lexer that skips trivia (comments, NL).
    pub fn new(src: &'src [u8]) -> Self {
        let truncated = src.len() > MAX_SOURCE_LEN;
        let src = if truncated { &src[..MAX_SOURCE_LEN] } else { src };
        let bom = src.starts_with(b"\xEF\xBB\xBF");
        Lexer {
            src,
            st: State {
                pos: if bom { 3 } else { 0 },
                depth: 0,
                kinds: 0,
                pending_dedents: 0,
                indent_len: 1,
                modes_len: 0,
                pending_error: None,
                at_bol: true,
                blank: false,
                finished: false,
                modes: [Frame::default(); MAX_MODES],
            },
            indent_gen: 0,
            trivia: false,
            truncated,
            suppressions: Vec::new(),
            indent: [Level::default(); MAX_INDENT],
        }
    }

    /// Lexer that also yields [`TokenKind::Comment`] and [`TokenKind::NonLogicalNewline`].
    pub fn with_trivia(src: &'src [u8]) -> Self {
        let mut lx = Lexer::new(src);
        lx.trivia = true;
        lx
    }

    /// The (possibly truncated) source being lexed.
    #[inline]
    pub fn source(&self) -> &'src [u8] {
        self.src
    }

    /// Input exceeded [`MAX_SOURCE_LEN`] and was truncated.
    #[inline]
    pub fn truncated(&self) -> bool {
        self.truncated
    }

    /// Byte offset where the next token scan starts.
    #[inline]
    pub fn pos(&self) -> u32 {
        self.st.pos
    }

    /// Current bracket nesting depth.
    #[inline]
    pub fn bracket_depth(&self) -> u32 {
        self.st.depth
    }

    /// Suppression comments seen so far, in source order.
    #[inline]
    pub fn suppressions(&self) -> &[Suppression] {
        &self.suppressions
    }

    /// Moves out the recorded suppressions.
    pub fn take_suppressions(&mut self) -> Vec<Suppression> {
        std::mem::take(&mut self.suppressions)
    }

    /// Saves the lexer state after the last returned token. Cheap: a fixed-size
    /// copy (position, bracket state, f-string mode stack).
    ///
    /// The indent stack is not saved: it is invariant within a logical line, so
    /// restore only between tokens of the same logical line (no NEWLINE/INDENT/
    /// DEDENT returned since the checkpoint was taken).
    #[inline]
    pub fn checkpoint(&self) -> Checkpoint {
        Checkpoint { st: self.st, suppressions: self.suppressions.len() as u32, indent_gen: self.indent_gen }
    }

    /// Rewinds to `cp`; the token stream from there is reproduced exactly.
    /// Suppressions recorded after the checkpoint are dropped (they are re-recorded on rescan).
    #[inline]
    pub fn restore(&mut self, cp: Checkpoint) {
        debug_assert_eq!(cp.indent_gen, self.indent_gen, "restore across a change of indentation");
        self.st = cp.st;
        self.suppressions.truncate(cp.suppressions as usize);
    }

    /// Parser error recovery for an unclosed bracket: forgets all open brackets
    /// and resumes scanning at `pos`, which must be the start of a token already
    /// returned on the current logical line. Indentation of the line containing
    /// `pos` is not reprocessed. Returns false (and changes nothing) inside an
    /// f/t-string or when `pos` is past the current position.
    pub fn recover_at(&mut self, pos: u32) -> bool {
        if self.st.modes_len != 0 || pos > self.st.pos {
            return false;
        }
        self.st.pos = pos;
        self.st.depth = 0;
        self.st.kinds = 0;
        self.st.pending_error = None;
        self.st.at_bol = false;
        self.st.blank = false;
        // Comments after `pos` are lexed (and their suppressions recorded) again.
        let keep = self.suppressions.partition_point(|s| s.start < pos);
        self.suppressions.truncate(keep);
        true
    }

    #[inline(always)]
    fn tok(&self, kind: TokenKind, start: usize, end: usize) -> Token {
        Token { start: start as u32, len: (end - start) as u32, kind }
    }

    /// Zero-length token at the current position.
    #[inline(always)]
    fn tok0(&self, kind: TokenKind) -> Token {
        Token { start: self.st.pos, len: 0, kind }
    }

    /// Ends a token at `end` and moves the scan position there.
    #[inline(always)]
    fn emit(&mut self, kind: TokenKind, start: usize, end: usize) -> Token {
        self.st.pos = end as u32;
        self.tok(kind, start, end)
    }

    #[inline(always)]
    fn at(&self, p: usize) -> Option<u8> {
        self.src.get(p).copied()
    }

    /// Returns the next token.
    #[inline]
    pub fn next_token(&mut self) -> Token {
        loop {
            // One combined test keeps the common path (a plain token mid-line) short.
            let st = &self.st;
            if st.at_bol | (st.pending_dedents != 0) | st.pending_error.is_some() | (st.modes_len != 0)
                && let Some(t) = self.special()
            {
                return t;
            }
            if let Some(t) = self.lex_token() {
                return t;
            }
        }
    }

    /// Queued DEDENTs/errors, f/t-string literal and format-spec modes, and line starts.
    /// `None` means the next token is lexed normally by [`Lexer::lex_token`].
    #[inline(never)]
    fn special(&mut self) -> Option<Token> {
        loop {
            if self.st.pending_dedents > 0 {
                self.st.pending_dedents -= 1;
                return Some(self.tok0(TokenKind::Dedent));
            }
            if let Some(k) = self.st.pending_error.take() {
                return Some(self.tok0(k));
            }
            if self.st.modes_len > 0 {
                let top = self.st.modes[self.st.modes_len as usize - 1];
                if top.flags & F_FIELD == 0 {
                    return Some(self.fstring_literal(top));
                }
                if top.flags & F_SPEC != 0 {
                    return Some(self.format_spec(top));
                }
            }
            if !self.st.at_bol {
                return None;
            }
            if let Some(t) = self.line_start() {
                return Some(t);
            }
        }
    }

    /// Lexes one token in normal (or f-string expression) mode. `None` means
    /// trivia was skipped; the caller loops.
    #[inline(always)]
    fn lex_token(&mut self) -> Option<Token> {
        use TokenKind as K;
        let src = self.src;
        let mut p = self.st.pos as usize;
        while let Some(b' ' | b'\t' | b'\x0c') = src.get(p) {
            p += 1;
        }
        self.st.pos = p as u32;
        let Some(&b) = src.get(p) else {
            return Some(self.eof());
        };
        let next = self.at(p + 1);
        let t = match b {
            b'a'..=b'z' | b'A'..=b'Z' | b'_' | 0x80..=0xff => self.name_or_string(p),
            b'0'..=b'9' => self.number(p),
            b'"' | b'\'' => self.string(p, p, 0),
            b'\n' | b'\r' => {
                let end = if b == b'\r' && next == Some(b'\n') { p + 2 } else { p + 1 };
                self.st.pos = end as u32;
                if self.st.depth == 0 {
                    self.st.at_bol = true;
                    if !self.st.blank {
                        return Some(self.tok(K::Newline, p, end));
                    }
                }
                if !self.trivia {
                    return None;
                }
                self.tok(K::NonLogicalNewline, p, end)
            }
            b'#' => return self.comment(p),
            b'(' => self.open(K::LPar, B_PAREN, p),
            b'[' => self.open(K::LSqb, B_SQB, p),
            b'{' => self.open(K::LBrace, B_BRACE, p),
            b')' => self.close(K::RPar, p),
            b']' => self.close(K::RSqb, p),
            b'}' => self.close_brace(p),
            b':' => {
                if let Some(f) = self.top_field()
                    && self.st.depth == f.depth + 1
                {
                    // Top-level `:` of a replacement field starts its format spec (even before `=`).
                    self.st.modes[self.st.modes_len as usize - 1].flags |= F_SPEC;
                    return Some(self.emit(K::Colon, p, p + 1));
                }
                self.op2(p, next, b'=', K::ColonEqual, K::Colon)
            }
            b',' => self.emit(K::Comma, p, p + 1),
            b';' => self.emit(K::Semi, p, p + 1),
            b'~' => self.emit(K::Tilde, p, p + 1),
            b'.' => match next {
                Some(b'0'..=b'9') => self.number(p),
                Some(b'.') if self.at(p + 2) == Some(b'.') => self.emit(K::Ellipsis, p, p + 3),
                _ => self.emit(K::Dot, p, p + 1),
            },
            b'+' => self.op2(p, next, b'=', K::PlusEqual, K::Plus),
            b'-' => match next {
                Some(b'=') => self.emit(K::MinusEqual, p, p + 2),
                Some(b'>') => self.emit(K::RArrow, p, p + 2),
                _ => self.emit(K::Minus, p, p + 1),
            },
            b'*' => self.op3(p, next, b'*', K::DoubleStar, K::DoubleStarEqual, K::StarEqual, K::Star),
            b'/' => self.op3(p, next, b'/', K::DoubleSlash, K::DoubleSlashEqual, K::SlashEqual, K::Slash),
            b'<' => self.op3(p, next, b'<', K::LeftShift, K::LeftShiftEqual, K::LessEqual, K::Less),
            b'>' => self.op3(p, next, b'>', K::RightShift, K::RightShiftEqual, K::GreaterEqual, K::Greater),
            b'=' => self.op2(p, next, b'=', K::EqEqual, K::Equal),
            b'!' => self.op2(p, next, b'=', K::NotEqual, K::Exclamation),
            b'%' => self.op2(p, next, b'=', K::PercentEqual, K::Percent),
            b'&' => self.op2(p, next, b'=', K::AmperEqual, K::Amper),
            b'|' => self.op2(p, next, b'=', K::VBarEqual, K::VBar),
            b'^' => self.op2(p, next, b'=', K::CircumflexEqual, K::Circumflex),
            b'@' => self.op2(p, next, b'=', K::AtEqual, K::At),
            b'\\' => match next {
                Some(b'\n') | Some(b'\r') => {
                    let end = if next == Some(b'\r') && self.at(p + 2) == Some(b'\n') { p + 3 } else { p + 2 };
                    self.st.pos = end as u32;
                    if end < src.len() {
                        return None;
                    }
                    // Continuation into EOF.
                    self.tok(K::ErrorBadContinuation, p, p + 1)
                }
                _ => self.emit(K::ErrorBadContinuation, p, p + 1),
            },
            _ => self.emit(K::ErrorUnknownChar, p, p + 1),
        };
        Some(t)
    }

    /// Comment at `p` up to the line terminator; records suppressions. `None` unless trivia mode.
    fn comment(&mut self, p: usize) -> Option<Token> {
        let src = self.src;
        let end = find2(src, p + 1, b'\n', b'\r').unwrap_or(src.len());
        suppress::scan(src, p, end, &mut self.suppressions);
        self.st.pos = end as u32;
        if !self.trivia {
            return None;
        }
        Some(self.tok(TokenKind::Comment, p, end))
    }

    /// `X` or `X=`-style two-way operator.
    #[inline(always)]
    fn op2(&mut self, p: usize, next: Option<u8>, c: u8, long: TokenKind, short: TokenKind) -> Token {
        if next == Some(c) { self.emit(long, p, p + 2) } else { self.emit(short, p, p + 1) }
    }

    /// `XX=`, `XX`, `X=`, `X` (for `*`, `/`, `<`, `>`).
    #[inline(always)]
    #[allow(clippy::too_many_arguments)]
    fn op3(&mut self, p: usize, next: Option<u8>, c: u8, dbl: TokenKind, dbl_eq: TokenKind, eq: TokenKind, single: TokenKind) -> Token {
        if next == Some(c) {
            if self.at(p + 2) == Some(b'=') { self.emit(dbl_eq, p, p + 3) } else { self.emit(dbl, p, p + 2) }
        } else if next == Some(b'=') {
            self.emit(eq, p, p + 2)
        } else {
            self.emit(single, p, p + 1)
        }
    }

    #[inline(always)]
    fn open(&mut self, kind: TokenKind, bk: u64, p: usize) -> Token {
        self.st.depth = self.st.depth.saturating_add(1);
        self.st.kinds = (self.st.kinds << 2) | bk;
        self.emit(kind, p, p + 1)
    }

    /// Lowest depth a closer may pop to: inside a replacement field its own `{`
    /// stays open (a stray `)` cannot close it).
    #[inline(always)]
    fn floor(&self) -> u32 {
        match self.top_field() {
            Some(f) => f.depth + 1,
            None => 0,
        }
    }

    #[inline(always)]
    fn close(&mut self, kind: TokenKind, p: usize) -> Token {
        if self.st.depth > self.floor() {
            self.st.depth -= 1;
            self.st.kinds >>= 2;
        }
        self.emit(kind, p, p + 1)
    }

    fn close_brace(&mut self, p: usize) -> Token {
        if let Some(f) = self.top_field() {
            let inner = self.st.kinds & 3;
            // Closes the replacement field when its `{` is innermost, or (recovery) when the
            // innermost open bracket is a known `(`/`[` that will never be closed.
            if self.st.depth <= f.depth + 1 || (inner != 0 && inner != B_BRACE) {
                self.pop_mode();
                self.set_depth(f.depth);
                return self.emit(TokenKind::RBrace, p, p + 1);
            }
        }
        self.close(TokenKind::RBrace, p)
    }

    /// Top mode frame if it is a replacement field (in expression mode when reached from `lex_token`).
    #[inline(always)]
    fn top_field(&self) -> Option<Frame> {
        if self.st.modes_len == 0 {
            return None;
        }
        let f = self.st.modes[self.st.modes_len as usize - 1];
        if f.flags & F_FIELD != 0 { Some(f) } else { None }
    }

    #[inline]
    fn pop_mode(&mut self) {
        self.st.modes_len -= 1;
    }

    /// Pops brackets down to depth `d` (`d <= depth`).
    #[inline]
    fn set_depth(&mut self, d: u32) {
        let diff = self.st.depth.saturating_sub(d);
        self.st.kinds = if diff >= 32 { 0 } else { self.st.kinds >> (2 * diff) };
        self.st.depth = d;
    }

    /// Error recovery: drops frames up to and including the innermost string
    /// frame and restores the bracket depth from before that string.
    fn abandon_fstring(&mut self) {
        while self.st.modes_len > 0 {
            let f = self.st.modes[self.st.modes_len as usize - 1];
            self.st.modes_len -= 1;
            if f.flags & F_FIELD == 0 {
                self.set_depth(f.depth);
                return;
            }
        }
    }

    /// End of input: unterminated f-string, final NEWLINE, outstanding DEDENTs, EOF.
    fn eof(&mut self) -> Token {
        if self.st.modes_len > 0 {
            // EOF inside a replacement field expression.
            self.st.modes_len = 0;
            self.set_depth(0);
            return self.tok0(TokenKind::ErrorUnterminatedFString);
        }
        if !self.st.at_bol && !self.st.blank {
            self.st.at_bol = true;
            self.set_depth(0);
            return self.tok0(TokenKind::Newline);
        }
        while self.st.indent_len > 1 {
            self.st.indent_len -= 1;
            self.indent_gen = self.indent_gen.wrapping_add(1);
            if !self.indent[self.st.indent_len as usize].phantom {
                return self.tok0(TokenKind::Dedent);
            }
        }
        self.tok0(TokenKind::EndOfFile)
    }

    /// Processes indentation at the start of a physical line (bracket depth 0).
    /// Returns an INDENT, or queues DEDENTs / errors and returns `None`.
    fn line_start(&mut self) -> Option<Token> {
        self.st.at_bol = false;
        let src = self.src;
        let start = self.st.pos as usize;
        // Spaces only is the common case; tabs and form feeds take the loop.
        let mut p = skip_spaces(src, start);
        let mut col = (p - start).min(u32::MAX as usize) as u32;
        let mut alt = col;
        loop {
            match src.get(p) {
                Some(b' ') => {
                    col = col.saturating_add(1);
                    alt = alt.saturating_add(1);
                }
                Some(b'\t') => {
                    col = (col / 8).saturating_add(1).saturating_mul(8);
                    alt = alt.saturating_add(1);
                }
                Some(b'\x0c') => {
                    col = 0;
                    alt = 0;
                }
                _ => break,
            }
            p += 1;
        }
        self.st.pos = p as u32;
        if let None | Some(b'#' | b'\n' | b'\r') = src.get(p) {
            // Blank or comment-only line: no indentation change, NL instead of NEWLINE.
            self.st.blank = true;
            return None;
        }
        self.st.blank = false;
        let len = self.st.indent_len as usize;
        let top = self.indent[len - 1];
        if col == top.col {
            if alt != top.alt {
                self.st.pending_error = Some(TokenKind::ErrorTabs);
            }
            return None;
        }
        if col > top.col {
            if len >= MAX_INDENT {
                self.st.pending_error = Some(TokenKind::ErrorTooDeep);
                return None;
            }
            if alt <= top.alt {
                self.st.pending_error = Some(TokenKind::ErrorTabs);
            }
            self.indent[len] = Level { col, alt, phantom: false };
            self.st.indent_len += 1;
            self.indent_gen = self.indent_gen.wrapping_add(1);
            return Some(self.tok(TokenKind::Indent, start, p));
        }
        let mut len = len;
        let mut dedents = 0;
        while len > 1 && self.indent[len - 1].col > col {
            len -= 1;
            dedents += u32::from(!self.indent[len].phantom);
        }
        self.st.pending_dedents = dedents;
        self.st.indent_len = len as u32;
        self.indent_gen = self.indent_gen.wrapping_add(1);
        let top = self.indent[len - 1];
        if top.col != col {
            // Dedent to a column that matches no outer level (IndentationError). Recovery:
            // the line belongs to the enclosing level, and a phantom level at its column
            // keeps following lines at the same column from becoming an INDENT.
            self.st.pending_error = Some(TokenKind::ErrorDedent);
            if len < MAX_INDENT {
                self.indent[len] = Level { col, alt, phantom: true };
                self.st.indent_len += 1;
            }
        } else if top.alt != alt {
            self.st.pending_error = Some(TokenKind::ErrorTabs);
        }
        None
    }

    fn name_or_string(&mut self, start: usize) -> Token {
        let src = self.src;
        let p = skip_ident(src, start + 1);
        if p - start <= 2
            && let Some(b'"' | b'\'') = src.get(p)
            && let Some(flags) = prefix_flags(&src[start..p])
        {
            return self.string(start, p, flags);
        }
        let kind = token::keyword(src, start, p);
        self.emit(kind, start, p)
    }

    /// String starting at `start` (prefix) whose opening quote is at `q`.
    fn string(&mut self, start: usize, qpos: usize, prefix: u8) -> Token {
        use TokenKind as K;
        let src = self.src;
        let q = src[qpos];
        let triple = self.at(qpos + 1) == Some(q) && self.at(qpos + 2) == Some(q);
        let body = if triple { qpos + 3 } else { qpos + 1 };
        if prefix & (P_F | P_T) != 0 {
            // A string frame plus at least one field frame must fit.
            if (self.st.modes_len as usize) + 2 <= MAX_MODES {
                let mut flags = 0;
                if triple {
                    flags |= F_TRIPLE;
                }
                if q == b'"' {
                    flags |= F_DQUOTE;
                }
                if prefix & P_RAW != 0 {
                    flags |= F_RAW;
                }
                if prefix & P_T != 0 {
                    flags |= F_TSTR;
                }
                self.st.modes[self.st.modes_len as usize] = Frame { depth: self.st.depth, flags };
                self.st.modes_len += 1;
                let kind = if prefix & P_T != 0 { K::TStringStart } else { K::FStringStart };
                return self.emit(kind, start, body);
            }
            // Nesting too deep: skip the string as if it had no replacement fields.
            let t = self.plain_string(start, body, q, triple);
            return Token { kind: K::ErrorFString, ..t };
        }
        let t = self.plain_string(start, body, q, triple);
        if t.kind == K::ErrorUnterminatedString
            && let Some(f) = self.top_field()
            && !f.triple()
        {
            return self.recover_field_string(t, f, qpos, q);
        }
        t
    }

    /// Error recovery for an unterminated string inside a replacement field of a
    /// single-quoted f/t-string. Valid code never has one, and without recovery the
    /// rest of the file would be lexed as part of the field.
    ///
    /// `f"{x"`: an unprefixed quote equal to the owner's quote closes the field
    /// (zero-length [`TokenKind::ErrorFString`]) and literal mode then ends the string
    /// at that quote, like CPython's "f-string: expecting '}'". Otherwise the whole
    /// f-string is abandoned after the error token.
    #[cold]
    fn recover_field_string(&mut self, t: Token, f: Frame, qpos: usize, q: u8) -> Token {
        if t.start as usize == qpos && q == f.quote() {
            self.st.pos = qpos as u32;
            self.pop_mode();
            self.set_depth(f.depth);
            return self.tok0(TokenKind::ErrorFString);
        }
        self.abandon_fstring();
        t
    }

    fn plain_string(&mut self, start: usize, body: usize, q: u8, triple: bool) -> Token {
        use TokenKind as K;
        let src = self.src;
        let n = src.len();
        let mut p = body;
        if triple {
            loop {
                let Some(i) = find2(src, p, q, b'\\') else {
                    return self.emit(K::ErrorUnterminatedTripleString, start, n);
                };
                if src[i] == b'\\' {
                    p = i + 2;
                } else if self.at(i + 1) == Some(q) && self.at(i + 2) == Some(q) {
                    return self.emit(K::String, start, i + 3);
                } else {
                    p = i + 1;
                }
                if p >= n {
                    return self.emit(K::ErrorUnterminatedTripleString, start, n);
                }
            }
        }
        loop {
            let Some(i) = find4(src, p, q, b'\\', b'\n', b'\r') else {
                return self.emit(K::ErrorUnterminatedString, start, n);
            };
            match src[i] {
                b'\\' => {
                    // Escaped byte; an escaped CRLF is one line continuation.
                    p = if self.at(i + 1) == Some(b'\r') && self.at(i + 2) == Some(b'\n') { i + 3 } else { i + 2 };
                    if p >= n {
                        return self.emit(K::ErrorUnterminatedString, start, n);
                    }
                }
                b'\n' | b'\r' => return self.emit(K::ErrorUnterminatedString, start, i),
                _ => return self.emit(K::String, start, i + 1),
            }
        }
    }

    /// Skips a backslash escape at `p` inside an f-string literal or format spec.
    /// A backslash before `{`/`}` is a lone byte (the brace keeps its meaning);
    /// `\N{...}` in non-raw strings (literal text and format specs alike) is a named escape,
    /// not a replacement field.
    #[inline]
    fn skip_escape(&self, p: usize, fr: Frame) -> usize {
        match self.at(p + 1) {
            None | Some(b'{' | b'}') => p + 1,
            Some(b'N') if fr.flags & F_RAW == 0 && self.at(p + 2) == Some(b'{') => {
                let q = fr.quote();
                let mut i = p + 3;
                while let Some(c) = self.at(i) {
                    match c {
                        b'}' => return i + 1,
                        b'\n' | b'\r' | b'{' => break,
                        _ if c == q => break,
                        _ => i += 1,
                    }
                }
                p + 2
            }
            Some(b'\r') if self.at(p + 2) == Some(b'\n') => p + 3,
            Some(_) => p + 2,
        }
    }

    /// Whether the owning string's closing quote(s) start at `p` (`src[p]` is its quote byte).
    #[inline]
    fn closes(&self, p: usize, fr: Frame) -> bool {
        let q = fr.quote();
        !fr.triple() || (self.at(p + 1) == Some(q) && self.at(p + 2) == Some(q))
    }

    /// Opens a replacement field at the `{` at `p`.
    fn open_field(&mut self, p: usize, owner: Frame) -> Token {
        if self.st.modes_len as usize >= MAX_MODES {
            return self.emit(TokenKind::ErrorFString, p, p + 1);
        }
        self.st.modes[self.st.modes_len as usize] = Frame { depth: self.st.depth, flags: F_FIELD | (owner.flags & STR_BITS) };
        self.st.modes_len += 1;
        self.open(TokenKind::LBrace, B_BRACE, p)
    }

    /// Literal mode: the top frame is the string frame `fr`.
    fn fstring_literal(&mut self, fr: Frame) -> Token {
        use TokenKind as K;
        let src = self.src;
        let start = self.st.pos as usize;
        let q = fr.quote();
        let mid = fr.middle_kind();
        let mut p = start;
        loop {
            let Some(&b) = src.get(p) else {
                if p > start {
                    return self.emit(mid, start, p);
                }
                self.st.pos = p as u32;
                self.abandon_fstring();
                return self.tok0(K::ErrorUnterminatedFString);
            };
            match b {
                b'{' => {
                    if self.at(p + 1) == Some(b'{') {
                        p += 2;
                        continue;
                    }
                    if p > start {
                        return self.emit(mid, start, p);
                    }
                    return self.open_field(p, fr);
                }
                b'}' => {
                    if self.at(p + 1) == Some(b'}') {
                        p += 2;
                        continue;
                    }
                    if p > start {
                        return self.emit(mid, start, p);
                    }
                    // Single `}` is not allowed in the literal part.
                    return self.emit(K::ErrorFString, p, p + 1);
                }
                b'\\' => p = self.skip_escape(p, fr),
                b'\n' | b'\r' if !fr.triple() => {
                    if p > start {
                        return self.emit(mid, start, p);
                    }
                    self.abandon_fstring();
                    return self.tok0(K::ErrorUnterminatedFString);
                }
                _ if b == q && self.closes(p, fr) => {
                    if p > start {
                        return self.emit(mid, start, p);
                    }
                    self.pop_mode();
                    let end = if fr.triple() { p + 3 } else { p + 1 };
                    let kind = if fr.flags & F_TSTR != 0 { K::TStringEnd } else { K::FStringEnd };
                    return self.emit(kind, p, end);
                }
                _ => p += 1,
            }
        }
    }

    /// Format-spec mode: the top frame is the field frame `fr` with `F_SPEC` set.
    fn format_spec(&mut self, fr: Frame) -> Token {
        use TokenKind as K;
        let src = self.src;
        let start = self.st.pos as usize;
        let q = fr.quote();
        let mid = fr.middle_kind();
        let mut p = start;
        loop {
            let Some(&b) = src.get(p) else {
                if p > start {
                    return self.emit(mid, start, p);
                }
                self.st.pos = p as u32;
                self.abandon_fstring();
                return self.tok0(K::ErrorUnterminatedFString);
            };
            match b {
                b'{' => {
                    if p > start {
                        return self.emit(mid, start, p);
                    }
                    return self.open_field(p, fr);
                }
                b'}' => {
                    if p > start {
                        return self.emit(mid, start, p);
                    }
                    self.pop_mode();
                    self.set_depth(fr.depth);
                    return self.emit(K::RBrace, p, p + 1);
                }
                b'\\' => p = self.skip_escape(p, fr),
                b'\n' | b'\r' if !fr.triple() => {
                    // Newline in a single-quoted format spec: close the field; literal mode
                    // then reports the unterminated string.
                    if p > start {
                        return self.emit(mid, start, p);
                    }
                    self.pop_mode();
                    self.set_depth(fr.depth);
                    return self.tok0(K::ErrorFString);
                }
                _ if b == q && self.closes(p, fr) => {
                    // Closing quote before `}`: close the field; literal mode ends the string.
                    if p > start {
                        return self.emit(mid, start, p);
                    }
                    self.pop_mode();
                    self.set_depth(fr.depth);
                    return self.tok0(K::ErrorFString);
                }
                _ => p += 1,
            }
        }
    }

    /// Numeric literal starting at `start` (a digit, or `.` followed by a digit).
    fn number(&mut self, start: usize) -> Token {
        use TokenKind as K;
        let src = self.src;
        let mut p = start;
        let mut kind = K::Int;
        let is_dec = |c: u8| c.is_ascii_digit();
        let mut bad = if src[p] == b'0' && matches!(self.at(p + 1), Some(b'x' | b'X' | b'o' | b'O' | b'b' | b'B')) {
            let digit: fn(u8) -> bool = match src[p + 1] | 0x20 {
                b'x' => |c| c.is_ascii_hexdigit(),
                b'o' => |c| matches!(c, b'0'..=b'7'),
                _ => |c| matches!(c, b'0' | b'1'),
            };
            p += 2;
            let (end, any) = digits(src, p, digit, true);
            p = end;
            !any
        } else {
            let mut leading_zero_nonzero = false;
            if src[p] != b'.' {
                let (end, _) = digits(src, p, is_dec, false);
                leading_zero_nonzero = src[p] == b'0' && src[p..end].iter().any(|&c| matches!(c, b'1'..=b'9'));
                p = end;
                if self.at(p) == Some(b'.') {
                    p += 1;
                    kind = K::Float;
                    if self.at(p).is_some_and(is_dec) {
                        p = digits(src, p, is_dec, false).0;
                    }
                }
            } else {
                // `.5`: the caller checked the digit after the dot.
                kind = K::Float;
                p = digits(src, p + 1, is_dec, false).0;
            }
            if let Some(b'e' | b'E') = self.at(p) {
                let mut e = p + 1;
                if let Some(b'+' | b'-') = self.at(e) {
                    e += 1;
                }
                if self.at(e).is_some_and(is_dec) {
                    p = digits(src, e, is_dec, false).0;
                    kind = K::Float;
                }
            }
            if let Some(b'j' | b'J') = self.at(p) {
                p += 1;
                kind = K::Imaginary;
            }
            kind == K::Int && leading_zero_nonzero
        };
        // Identifier bytes glued to a number are an error, except a following keyword
        // (`1if x else 2`, `0x1for`), which CPython accepts.
        if let Some(c) = self.at(p)
            && IDENT[c as usize]
            && !starts_with_number_keyword(&src[p..])
        {
            bad = true;
            while self.at(p).is_some_and(|c| IDENT[c as usize]) {
                p += 1;
            }
        }
        self.emit(if bad { K::ErrorInvalidNumber } else { kind }, start, p)
    }

    /// Lexes the whole input into a vector (convenience for tests and tools;
    /// the parser pulls tokens instead). Includes the final EndOfFile.
    pub fn tokenize(mut self) -> (Vec<Token>, Vec<Suppression>) {
        // About one token per 8 source bytes in real code.
        let mut out = Vec::with_capacity(self.src.len() / 6 + 8);
        loop {
            let t = self.next_token();
            out.push(t);
            if t.kind == TokenKind::EndOfFile {
                break;
            }
        }
        (out, self.suppressions)
    }
}

impl Iterator for Lexer<'_> {
    type Item = Token;

    /// Yields tokens up to and including EndOfFile, then `None`.
    fn next(&mut self) -> Option<Token> {
        if self.st.finished {
            return None;
        }
        let t = self.next_token();
        if t.kind == TokenKind::EndOfFile {
            self.st.finished = true;
        }
        Some(t)
    }
}

/// Scans `digit (_? digit)*` from `p`. With `lead_underscore` (after a base prefix)
/// the first digit may also be preceded by `_`. Returns the end and whether any
/// digit was consumed. A trailing or doubled `_` is left unconsumed.
#[inline]
fn digits(src: &[u8], mut p: usize, digit: impl Fn(u8) -> bool, lead_underscore: bool) -> (usize, bool) {
    let mut any = false;
    loop {
        let save = p;
        if (any || lead_underscore) && src.get(p) == Some(&b'_') {
            p += 1;
        }
        if !src.get(p).is_some_and(|&c| digit(c)) {
            return (save, any);
        }
        while src.get(p).is_some_and(|&c| digit(c)) {
            p += 1;
        }
        any = true;
    }
}

/// CPython lets these keywords follow a number without a space (with a warning).
#[inline]
fn starts_with_number_keyword(s: &[u8]) -> bool {
    const KWS: [&[u8]; 8] = [b"and", b"else", b"for", b"if", b"in", b"is", b"not", b"or"];
    KWS.iter().any(|k| s.starts_with(k))
}

/// Valid string prefix (case-insensitive): r u b br rb f fr rf t tr rt.
#[inline]
fn prefix_flags(w: &[u8]) -> Option<u8> {
    let lower = |c: u8| c | 0x20;
    match *w {
        [a] => match lower(a) {
            b'r' => Some(P_RAW),
            b'u' | b'b' => Some(0),
            b'f' => Some(P_F),
            b't' => Some(P_T),
            _ => None,
        },
        [a, b] => match (lower(a), lower(b)) {
            (b'b', b'r') | (b'r', b'b') => Some(P_RAW),
            (b'f', b'r') | (b'r', b'f') => Some(P_F | P_RAW),
            (b't', b'r') | (b'r', b't') => Some(P_T | P_RAW),
            _ => None,
        },
        _ => None,
    }
}
