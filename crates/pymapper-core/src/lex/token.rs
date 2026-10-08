//! Token representation: 12 bytes, `kind` plus a byte span into the source.

use std::ops::Range;

/// Kind of a lexed token. One byte; hard keywords get their own kinds so the
/// parser can dispatch without comparing bytes. Soft keywords (`match`, `case`,
/// `type`, `_`) are lexed as [`TokenKind::Name`].
///
/// Variants are grouped in contiguous discriminant ranges (keywords, operators,
/// errors) so the classification predicates are single comparisons.
#[repr(u8)]
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum TokenKind {
    // Structure.
    /// End of input. Zero length, at `src.len()`. Repeats forever once reached.
    EndOfFile = 0,
    /// End of a logical line (`NEWLINE`). Zero length at EOF when the last line has no terminator.
    Newline,
    /// Newline that does not end a logical line (`NL`): blank lines and newlines inside brackets.
    /// Only emitted in trivia mode.
    NonLogicalNewline,
    /// Indentation increase; spans the leading whitespace of the line.
    Indent,
    /// Indentation decrease; zero length at the first token of the line.
    Dedent,
    /// `# ...` up to (not including) the line terminator. Only emitted in trivia mode.
    Comment,

    // Atoms.
    Name,
    Int,
    Float,
    Imaginary,
    /// A complete non-f/t string literal including prefix and quotes.
    String,
    /// `f"` / `rf'''` etc.: prefix plus opening quote(s).
    FStringStart,
    /// Literal text inside an f-string (or its format spec). Raw span; `{{`/`}}` are not unescaped.
    FStringMiddle,
    /// Closing quote(s) of an f-string.
    FStringEnd,
    TStringStart,
    TStringMiddle,
    TStringEnd,

    // Hard keywords.
    False,
    None,
    True,
    And,
    As,
    Assert,
    Async,
    Await,
    Break,
    Class,
    Continue,
    Def,
    Del,
    Elif,
    Else,
    Except,
    Finally,
    For,
    From,
    Global,
    If,
    Import,
    In,
    Is,
    Lambda,
    Nonlocal,
    Not,
    Or,
    Pass,
    Raise,
    Return,
    Try,
    While,
    With,
    Yield,

    // Operators and delimiters.
    LPar,
    RPar,
    LSqb,
    RSqb,
    LBrace,
    RBrace,
    Colon,
    Comma,
    Semi,
    Plus,
    Minus,
    Star,
    Slash,
    VBar,
    Amper,
    Less,
    Greater,
    Equal,
    Dot,
    Percent,
    EqEqual,
    NotEqual,
    LessEqual,
    GreaterEqual,
    Tilde,
    Circumflex,
    LeftShift,
    RightShift,
    DoubleStar,
    PlusEqual,
    MinusEqual,
    StarEqual,
    SlashEqual,
    PercentEqual,
    AmperEqual,
    VBarEqual,
    CircumflexEqual,
    LeftShiftEqual,
    RightShiftEqual,
    DoubleStarEqual,
    DoubleSlash,
    DoubleSlashEqual,
    At,
    AtEqual,
    RArrow,
    Ellipsis,
    ColonEqual,
    /// `!`: f-string conversion marker (`{x!r}`); invalid elsewhere (the parser reports it).
    Exclamation,

    // Errors. The lexer never aborts: invalid input yields one of these and lexing continues.
    /// A byte that cannot start any token (`$`, `?`, `` ` ``, NUL, other control bytes).
    ErrorUnknownChar,
    /// Single-quoted string not closed before the end of the line; spans up to the terminator.
    ErrorUnterminatedString,
    /// Triple-quoted string not closed before EOF; spans to EOF.
    ErrorUnterminatedTripleString,
    /// f/t-string literal not closed (newline in a single-quoted one, or EOF). Zero length.
    ErrorUnterminatedFString,
    /// Other f/t-string structure errors: single `}`, missing `}` before the closing quote,
    /// newline in a single-quoted format spec, nesting deeper than the mode stack.
    ErrorFString,
    /// Malformed numeric literal (`1_`, `0x`, `0b2`, `0777`, `1abc`); spans the number and
    /// any identifier bytes glued to it.
    ErrorInvalidNumber,
    /// `\` not followed by a line terminator (or followed by EOF).
    ErrorBadContinuation,
    /// Unindent to a column that matches no outer indentation level. Zero length.
    ErrorDedent,
    /// Indentation that is inconsistent between tab-size 8 and tab-size 1 (Python's `TabError`).
    /// Zero length.
    ErrorTabs,
    /// Indentation nested deeper than the lexer's fixed indent stack. Zero length.
    ErrorTooDeep,
}

impl TokenKind {
    /// Hard keyword (`if`, `def`, `None`, ...).
    #[inline]
    pub const fn is_keyword(self) -> bool {
        (self as u8) >= (TokenKind::False as u8) && (self as u8) <= (TokenKind::Yield as u8)
    }

    /// Operator or delimiter.
    #[inline]
    pub const fn is_operator(self) -> bool {
        (self as u8) >= (TokenKind::LPar as u8) && (self as u8) <= (TokenKind::Exclamation as u8)
    }

    /// One of the `Error*` kinds.
    #[inline]
    pub const fn is_error(self) -> bool {
        (self as u8) >= (TokenKind::ErrorUnknownChar as u8)
    }

    /// Tokens that carry no syntax for the parser (only produced in trivia mode).
    #[inline]
    pub const fn is_trivia(self) -> bool {
        matches!(self, TokenKind::Comment | TokenKind::NonLogicalNewline)
    }
}

/// A token: kind plus byte span. 12 bytes; the span indexes the original `&[u8]`.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Token {
    pub start: u32,
    pub len: u32,
    pub kind: TokenKind,
}

const _: () = assert!(std::mem::size_of::<Token>() == 12);
const _: () = assert!(std::mem::size_of::<TokenKind>() == 1);

impl Token {
    /// Exclusive end offset.
    #[inline]
    pub const fn end(self) -> u32 {
        self.start + self.len
    }

    #[inline]
    pub const fn range(self) -> Range<usize> {
        self.start as usize..self.end() as usize
    }

    /// The token's bytes. `src` must be the slice the token was lexed from.
    #[inline]
    pub fn text(self, src: &[u8]) -> &[u8] {
        &src[self.range()]
    }
}

/// Hard keywords and their kinds (the source of the perfect-hash table below).
const KEYWORDS: [(&[u8], TokenKind); 35] = {
    use TokenKind as K;
    [
        (b"False", K::False), (b"None", K::None), (b"True", K::True), (b"and", K::And), (b"as", K::As),
        (b"assert", K::Assert), (b"async", K::Async), (b"await", K::Await), (b"break", K::Break),
        (b"class", K::Class), (b"continue", K::Continue), (b"def", K::Def), (b"del", K::Del),
        (b"elif", K::Elif), (b"else", K::Else), (b"except", K::Except), (b"finally", K::Finally),
        (b"for", K::For), (b"from", K::From), (b"global", K::Global), (b"if", K::If),
        (b"import", K::Import), (b"in", K::In), (b"is", K::Is), (b"lambda", K::Lambda),
        (b"nonlocal", K::Nonlocal), (b"not", K::Not), (b"or", K::Or), (b"pass", K::Pass),
        (b"raise", K::Raise), (b"return", K::Return), (b"try", K::Try), (b"while", K::While),
        (b"with", K::With), (b"yield", K::Yield),
    ]
};

/// Multiplier of the keyword hash: `(word * KW_MUL) >> 58` is collision-free over
/// [`KEYWORDS`] (checked at compile time). Found by random search.
const KW_MUL: u64 = 0xb57c_8e05_e3c9_b0e1;

#[inline(always)]
const fn kw_slot(word: u64) -> usize {
    (word.wrapping_mul(KW_MUL) >> 58) as usize
}

/// Keyword bytes as a little-endian zero-padded `u64` per hash slot (0 = empty), and the kind.
/// Identifier bytes are never 0, so a word of 2..=8 bytes is uniquely identified by its `u64`.
static KW_TABLE: ([u64; 64], [TokenKind; 64]) = {
    let mut words = [0u64; 64];
    let mut kinds = [TokenKind::Name; 64];
    let mut i = 0;
    while i < KEYWORDS.len() {
        let (kw, kind) = KEYWORDS[i];
        let mut a = [0u8; 8];
        let mut j = 0;
        while j < kw.len() {
            a[j] = kw[j];
            j += 1;
        }
        let w = u64::from_le_bytes(a);
        let slot = kw_slot(w);
        assert!(words[slot] == 0, "keyword hash collision: pick another KW_MUL");
        words[slot] = w;
        kinds[slot] = kind;
        i += 1;
    }
    (words, kinds)
};

/// Maps the identifier `src[start..end]` to its hard-keyword kind, or [`TokenKind::Name`].
#[inline(always)]
pub(super) fn keyword(src: &[u8], start: usize, end: usize) -> TokenKind {
    let len = end - start;
    // Keywords are 2..=8 bytes.
    if len.wrapping_sub(2) > 6 {
        return TokenKind::Name;
    }
    let mut a = [0u8; 8];
    let w = match src.get(start..start + 8) {
        Some(chunk) => {
            a.copy_from_slice(chunk);
            u64::from_le_bytes(a) & (u64::MAX >> (64 - 8 * len))
        }
        None => {
            a[..len].copy_from_slice(&src[start..end]);
            u64::from_le_bytes(a)
        }
    };
    let slot = kw_slot(w);
    if KW_TABLE.0[slot] == w { KW_TABLE.1[slot] } else { TokenKind::Name }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kw(s: &[u8]) -> TokenKind {
        keyword(s, 0, s.len())
    }

    #[test]
    fn keyword_table_matches_list() {
        for (w, k) in KEYWORDS {
            assert_eq!(kw(w), k);
            // With trailing bytes after the word (the 8-byte load path).
            let mut padded = w.to_vec();
            padded.extend_from_slice(b"xxxxxxxx");
            assert_eq!(keyword(&padded, 0, w.len()), k);
            // Prefixes and extensions are names.
            assert_eq!(kw(&w[..w.len() - 1]), TokenKind::Name);
            assert_eq!(keyword(&padded, 0, w.len() + 1), TokenKind::Name);
        }
        for w in [&b""[..], b"x", b"match", b"case", b"type", b"_", b"elsewhere", b"nonlocals", b"Nonlocal", b"\xffif"] {
            assert_eq!(kw(w), TokenKind::Name, "{w:?}");
        }
    }
}
