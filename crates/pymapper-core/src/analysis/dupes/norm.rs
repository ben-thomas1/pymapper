//! Token normalisation shared by fingerprinting (parser token events) and match extension
//! (lexer re-scan): both must produce the same code sequence for the same file.

use crate::intern::hash_bytes;
use crate::lex::{Token, TokenKind};

/// Normalises a stream of non-structural tokens (no NEWLINE/INDENT/DEDENT/EOF, no errors, no
/// trivia) into one-byte codes:
///
/// - identifiers (soft keywords included) become one code, numbers another, string literals
///   and f/t-string literal text a third, f- and t-string delimiters one start and one end
///   code (so `f"..."` and `t'...'` match);
/// - keywords and operators keep their kind;
/// - statement-level `import` / `from` statements are dropped entirely: import blocks are
///   alike everywhere and would dominate the results.
///
/// Logical lines are recovered from the gaps between tokens (a line break that is not a
/// backslash continuation, outside brackets) since the token stream has no NEWLINE.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Normaliser {
    /// Bracket depth.
    depth: u32,
    /// End of the previous token (0 before the first).
    prev_end: u32,
    /// Kind of the previous token.
    prev_kind: TokenKind,
    /// A token was seen.
    started: bool,
    /// Inside a dropped import statement.
    skipping: bool,
}

impl Default for Normaliser {
    fn default() -> Self {
        Normaliser { depth: 0, prev_end: 0, prev_kind: TokenKind::EndOfFile, started: false, skipping: false }
    }
}

impl Normaliser {
    /// The code of `tok` (lexed from `src`), or `None` when it is dropped. Tokens must come in
    /// source order; a token that starts before the end of the previous one (parser error
    /// recovery re-scans) is dropped.
    #[inline]
    pub(crate) fn push(&mut self, src: &[u8], tok: Token) -> Option<u8> {
        use TokenKind as K;
        if tok.start < self.prev_end {
            return None;
        }
        // Statement starts matter only to enter or leave an import statement.
        let import = matches!(tok.kind, K::Import | K::From);
        if (import || self.skipping) && self.at_stmt_start(src, tok.start) {
            self.skipping = import;
        }
        self.started = true;
        self.prev_end = tok.end();
        self.prev_kind = tok.kind;
        match tok.kind {
            K::LPar | K::LSqb | K::LBrace => self.depth += 1,
            K::RPar | K::RSqb | K::RBrace => self.depth = self.depth.saturating_sub(1),
            _ => {}
        }
        if self.skipping { None } else { Some(code(tok.kind)) }
    }

    /// [`push`](Self::push) that also tells whether the token starts a statement (for
    /// trimming reported regions; slower, so not used while fingerprinting).
    #[inline]
    pub(crate) fn push_stmt(&mut self, src: &[u8], tok: Token) -> Option<(u8, bool)> {
        let starts = tok.start >= self.prev_end && self.at_stmt_start(src, tok.start);
        self.push(src, tok).map(|c| (c, starts))
    }

    /// A token starting at `start` begins a statement: first token, or outside brackets after
    /// `;`, `:` or a line break.
    #[inline]
    fn at_stmt_start(&self, src: &[u8], start: u32) -> bool {
        !self.started
            || (self.depth == 0
                && (matches!(self.prev_kind, TokenKind::Semi | TokenKind::Colon) || line_break(src, self.prev_end, start)))
    }
}

/// Normalised code of a token kind.
#[inline]
pub(crate) fn code(kind: TokenKind) -> u8 {
    use TokenKind as K;
    let k = match kind {
        K::Int | K::Float | K::Imaginary => K::Int,
        K::String | K::FStringMiddle | K::TStringMiddle => K::String,
        K::FStringStart | K::TStringStart => K::FStringStart,
        K::FStringEnd | K::TStringEnd => K::FStringEnd,
        k => k,
    };
    k as u8
}

/// Hash of the exact token (its bytes for names and literals, its kind otherwise): two tokens
/// with the same code are identical iff their exact hashes agree (up to hash collisions).
#[inline]
pub(crate) fn exact(src: &[u8], tok: Token) -> u32 {
    use TokenKind as K;
    match tok.kind {
        K::Name
        | K::Int
        | K::Float
        | K::Imaginary
        | K::String
        | K::FStringStart
        | K::FStringMiddle
        | K::FStringEnd
        | K::TStringStart
        | K::TStringMiddle
        | K::TStringEnd => {
            let h = hash_bytes(tok.text(src));
            (h ^ (h >> 32)) as u32
        }
        k => k as u32,
    }
}

/// The gap `src[from..to]` between two tokens holds a line break that is not a backslash
/// continuation. Gaps hold only blanks, comments, continuations and line breaks.
#[inline]
fn line_break(src: &[u8], from: u32, to: u32) -> bool {
    let gap = &src[(from as usize).min(src.len())..(to as usize).min(src.len())];
    let mut i = 0;
    while i < gap.len() {
        match gap[i] {
            b'\n' | b'\r' | b'#' => return true,
            b'\\' => {
                // Continuation: skip the line terminator after it.
                i += 1;
                if gap.get(i) == Some(&b'\r') {
                    i += 1;
                }
                if gap.get(i) == Some(&b'\n') {
                    i += 1;
                }
            }
            _ => i += 1,
        }
    }
    false
}
