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

/// Maps an identifier to its hard-keyword kind, or [`TokenKind::Name`].
#[inline]
pub(super) fn keyword(s: &[u8]) -> TokenKind {
    use TokenKind as K;
    // Keywords are 2..=8 bytes and start with a lowercase letter or F/N/T.
    if s.len() < 2 || s.len() > 8 {
        return K::Name;
    }
    match s {
        b"if" => K::If,
        b"in" => K::In,
        b"is" => K::Is,
        b"as" => K::As,
        b"or" => K::Or,
        b"def" => K::Def,
        b"for" => K::For,
        b"and" => K::And,
        b"not" => K::Not,
        b"del" => K::Del,
        b"try" => K::Try,
        b"None" => K::None,
        b"True" => K::True,
        b"else" => K::Else,
        b"elif" => K::Elif,
        b"from" => K::From,
        b"pass" => K::Pass,
        b"with" => K::With,
        b"False" => K::False,
        b"class" => K::Class,
        b"while" => K::While,
        b"yield" => K::Yield,
        b"raise" => K::Raise,
        b"break" => K::Break,
        b"async" => K::Async,
        b"await" => K::Await,
        b"return" => K::Return,
        b"import" => K::Import,
        b"except" => K::Except,
        b"global" => K::Global,
        b"lambda" => K::Lambda,
        b"assert" => K::Assert,
        b"finally" => K::Finally,
        b"continue" => K::Continue,
        b"nonlocal" => K::Nonlocal,
        _ => K::Name,
    }
}
