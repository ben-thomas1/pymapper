//! Lexer unit tests, grouped by token class and tricky cases.
//!
//! Expected streams for valid Python were cross-checked against CPython 3.14's
//! `tokenize` (see `xtask/scripts/lexdiff.py` for the corpus-wide comparison).

use super::*;
use TokenKind as K;

/// All tokens (trivia mode) as (kind, text), excluding the final EndOfFile.
fn toks(src: &str) -> Vec<(TokenKind, String)> {
    toks_bytes(src.as_bytes())
}

fn toks_bytes(src: &[u8]) -> Vec<(TokenKind, String)> {
    let (v, _) = Lexer::with_trivia(src).tokenize();
    assert_eq!(v.last().map(|t| t.kind), Some(K::EndOfFile));
    v[..v.len() - 1].iter().map(|t| (t.kind, String::from_utf8_lossy(t.text(src)).into_owned())).collect()
}

/// Kinds in parser mode (no trivia), excluding the final EndOfFile.
fn kinds(src: &str) -> Vec<TokenKind> {
    let (v, _) = Lexer::new(src.as_bytes()).tokenize();
    v[..v.len() - 1].iter().map(|t| t.kind).collect()
}

/// Kinds in trivia mode, excluding the final EndOfFile.
fn tkinds(src: &str) -> Vec<TokenKind> {
    toks(src).into_iter().map(|(k, _)| k).collect()
}

/// (kind, text) pairs from a compact list.
fn tt(v: &[(TokenKind, &str)]) -> Vec<(TokenKind, String)> {
    v.iter().map(|&(k, s)| (k, s.to_owned())).collect()
}

/// Kind of the single token of a one-token expression line.
fn one(src: &str) -> (TokenKind, String) {
    let v = toks(src);
    assert!(v.len() == 2 && v[1].0 == K::Newline, "{src:?} lexed as {v:?}");
    v[0].clone()
}

/// Invariants that must hold for every input: spans in bounds, non-decreasing starts,
/// no overlap, EOF last and repeated, INDENT/DEDENT balanced, checkpoint replays exactly.
fn check_invariants(src: &[u8]) {
    for trivia in [false, true] {
        let mut lx = if trivia { Lexer::with_trivia(src) } else { Lexer::new(src) };
        let mut prev_end = 0u32;
        let mut depth = 0i64;
        let mut n = 0usize;
        loop {
            let t = lx.next_token();
            n += 1;
            assert!(t.end() as usize <= src.len(), "token {t:?} out of bounds");
            assert!(t.start >= prev_end, "token {t:?} overlaps previous end {prev_end}");
            prev_end = t.end();
            match t.kind {
                K::Indent => depth += 1,
                K::Dedent => depth -= 1,
                _ => {}
            }
            assert!(depth >= 0, "DEDENT without INDENT in {src:?}");
            if t.kind == K::EndOfFile {
                assert_eq!(t.start as usize, src.len());
                break;
            }
            // Progress: at most a few zero-length tokens per byte (+ dedents at EOF).
            assert!(n <= 4 * src.len() + 2 * MAX_INDENT + 8, "no progress on {src:?}");
        }
        assert_eq!(depth, 0, "unbalanced INDENT/DEDENT in {src:?}");
        assert_eq!(lx.next_token().kind, K::EndOfFile);
        assert_eq!(lx.next_token().kind, K::EndOfFile);
    }
    // Checkpoint after every token of the first logical line replays the same tokens.
    let all: Vec<Token> = Lexer::new(src).tokenize().0;
    let mut lx = Lexer::new(src);
    let mut i = 0;
    while i < all.len() {
        let cp = lx.checkpoint();
        let a = lx.next_token();
        assert_eq!(a, all[i]);
        if matches!(a.kind, K::Newline | K::Indent | K::Dedent | K::EndOfFile) {
            break;
        }
        let b = lx.next_token();
        lx.restore(cp);
        assert_eq!(lx.next_token(), a);
        assert_eq!(lx.next_token(), b);
        lx.restore(cp);
        lx.next_token();
        i += 1;
    }
}

// ---------------------------------------------------------------------------
// Token representation.

#[test]
fn token_is_12_bytes() {
    assert_eq!(std::mem::size_of::<Token>(), 12);
    assert_eq!(std::mem::size_of::<TokenKind>(), 1);
}

#[test]
fn kind_classes() {
    assert!(K::False.is_keyword() && K::Yield.is_keyword() && K::If.is_keyword());
    assert!(!K::Name.is_keyword() && !K::LPar.is_keyword());
    assert!(K::LPar.is_operator() && K::Exclamation.is_operator() && K::ColonEqual.is_operator());
    assert!(!K::Yield.is_operator() && !K::ErrorUnknownChar.is_operator());
    assert!(K::ErrorUnknownChar.is_error() && K::ErrorTooDeep.is_error());
    assert!(!K::Exclamation.is_error());
    assert!(K::Comment.is_trivia() && K::NonLogicalNewline.is_trivia() && !K::Newline.is_trivia());
}

// ---------------------------------------------------------------------------
// Names and keywords.

#[test]
fn all_hard_keywords() {
    let words = [
        ("False", K::False), ("None", K::None), ("True", K::True), ("and", K::And), ("as", K::As),
        ("assert", K::Assert), ("async", K::Async), ("await", K::Await), ("break", K::Break),
        ("class", K::Class), ("continue", K::Continue), ("def", K::Def), ("del", K::Del),
        ("elif", K::Elif), ("else", K::Else), ("except", K::Except), ("finally", K::Finally),
        ("for", K::For), ("from", K::From), ("global", K::Global), ("if", K::If),
        ("import", K::Import), ("in", K::In), ("is", K::Is), ("lambda", K::Lambda),
        ("nonlocal", K::Nonlocal), ("not", K::Not), ("or", K::Or), ("pass", K::Pass),
        ("raise", K::Raise), ("return", K::Return), ("try", K::Try), ("while", K::While),
        ("with", K::With), ("yield", K::Yield),
    ];
    for (w, k) in words {
        assert_eq!(one(w), (k, w.to_owned()));
    }
}

#[test]
fn soft_keywords_and_near_keywords_are_names() {
    for w in ["match", "case", "type", "_", "none", "TRUE", "iff", "i", "elif_", "__init__", "print", "nonlocals", "x1"] {
        assert_eq!(one(w), (K::Name, w.to_owned()), "{w}");
    }
}

#[test]
fn non_ascii_and_non_utf8_identifiers() {
    assert_eq!(one("café"), (K::Name, "café".to_owned()));
    assert_eq!(one("变量"), (K::Name, "变量".to_owned()));
    // Invalid UTF-8 bytes are identifier bytes too; no validation.
    let v = toks_bytes(b"a\xff\xfeb = 1\n");
    assert_eq!(v[0].0, K::Name);
    assert_eq!(v[0].1.len(), "a\u{fffd}\u{fffd}b".len());
    assert_eq!(v[1].0, K::Equal);
}

#[test]
fn word_at_scan_paths_near_eof() {
    // Identifiers, keywords and indentation of every length up to past one 8-byte word, both
    // at EOF (short tail path) and followed by more input (word-at-a-time path).
    for n in 1..20 {
        let name = "a".repeat(n);
        for tail in ["", " = 1\n"] {
            let src = format!("{name}{tail}");
            assert_eq!(toks(&src)[0], (K::Name, name.clone()), "{src:?}");
            let indented = format!("if x:\n{}y{tail}", " ".repeat(n));
            let v = toks(&indented);
            assert_eq!(v[4], (K::Indent, " ".repeat(n)), "{indented:?}");
            assert_eq!(v[5], (K::Name, "y".to_owned()), "{indented:?}");
        }
    }
    for kw in ["if", "def", "None", "while", "return", "finally", "continue", "nonlocal"] {
        assert!(Lexer::new(kw.as_bytes()).next_token().kind.is_keyword(), "{kw}");
        assert!(Lexer::new(format!("{kw} ").as_bytes()).next_token().kind.is_keyword(), "{kw}");
        assert_eq!(Lexer::new(format!("{kw}x").as_bytes()).next_token().kind, K::Name, "{kw}");
    }
    // A non-ASCII byte right after the first word boundary.
    assert_eq!(toks("abcdefgh\u{e9}ij = 1\n")[0].1, "abcdefgh\u{e9}ij");
}

// ---------------------------------------------------------------------------
// Numbers.

#[test]
fn integers() {
    for s in ["0", "7", "1234567890", "1_000_000", "00", "0_0", "0x1F", "0XdeadBEEF", "0x_1", "0o17", "0O_7", "0b1010", "0B_1_0"] {
        assert_eq!(one(s), (K::Int, s.to_owned()), "{s}");
    }
}

#[test]
fn floats_and_imaginary() {
    for s in ["1.", "1.5", ".5", "1e5", "1E+5", "1e-5", "1.e5", "1_0.0_1e1_0", "0.0", "017.5", "017e1", "00.5", "1.5E-10"] {
        assert_eq!(one(s), (K::Float, s.to_owned()), "{s}");
    }
    for s in ["1j", "1J", "1.5j", ".5j", "1e5j", "017j", "0j"] {
        assert_eq!(one(s), (K::Imaginary, s.to_owned()), "{s}");
    }
}

#[test]
fn invalid_numbers() {
    for s in ["0777", "0_7", "1_", "1__0", "0x", "0b2", "0b12", "0o8", "1e", "1abc", "1.__class__", "0x_", "1e5_", "09"] {
        assert_eq!(one(s), (K::ErrorInvalidNumber, s.to_owned()), "{s}");
    }
}

#[test]
fn number_followed_by_keyword() {
    // CPython accepts these (with a SyntaxWarning).
    assert_eq!(tkinds("1if x else 2\n"), [K::Int, K::If, K::Name, K::Else, K::Int, K::Newline]);
    assert_eq!(tkinds("0x1for x in y\n")[..2], [K::Int, K::Or]);
    assert_eq!(toks("0x1for\n")[0].1, "0x1f");
    assert_eq!(tkinds("1jif 1 else 2\n")[..2], [K::Imaginary, K::If]);
    assert_eq!(tkinds("1or 2\n"), [K::Int, K::Or, K::Int, K::Newline]);
    assert_eq!(tkinds("1not in x\n")[..3], [K::Int, K::Not, K::In]);
}

#[test]
fn number_then_dot_attribute() {
    assert_eq!(tkinds("1 .real\n"), [K::Int, K::Dot, K::Name, K::Newline]);
    assert_eq!(tkinds("1..real\n"), [K::Float, K::Dot, K::Name, K::Newline]);
    assert_eq!(tkinds("1.5.3\n"), [K::Float, K::Float, K::Newline]);
    assert_eq!(tkinds("x[1:.5]\n"), [K::Name, K::LSqb, K::Int, K::Colon, K::Float, K::RSqb, K::Newline]);
}

// ---------------------------------------------------------------------------
// Operators.

#[test]
fn all_operators_longest_match() {
    let ops = [
        ("(", K::LPar), (")", K::RPar), ("[", K::LSqb), ("]", K::RSqb), ("{", K::LBrace), ("}", K::RBrace),
        (":", K::Colon), (",", K::Comma), (";", K::Semi), ("+", K::Plus), ("-", K::Minus), ("*", K::Star),
        ("/", K::Slash), ("|", K::VBar), ("&", K::Amper), ("<", K::Less), (">", K::Greater), ("=", K::Equal),
        (".", K::Dot), ("%", K::Percent), ("==", K::EqEqual), ("!=", K::NotEqual), ("<=", K::LessEqual),
        (">=", K::GreaterEqual), ("~", K::Tilde), ("^", K::Circumflex), ("<<", K::LeftShift),
        (">>", K::RightShift), ("**", K::DoubleStar), ("+=", K::PlusEqual), ("-=", K::MinusEqual),
        ("*=", K::StarEqual), ("/=", K::SlashEqual), ("%=", K::PercentEqual), ("&=", K::AmperEqual),
        ("|=", K::VBarEqual), ("^=", K::CircumflexEqual), ("<<=", K::LeftShiftEqual),
        (">>=", K::RightShiftEqual), ("**=", K::DoubleStarEqual), ("//", K::DoubleSlash),
        ("//=", K::DoubleSlashEqual), ("@", K::At), ("@=", K::AtEqual), ("->", K::RArrow),
        ("...", K::Ellipsis), (":=", K::ColonEqual), ("!", K::Exclamation),
    ];
    for (s, k) in ops {
        // Spaced out so brackets are balanced irrelevant: lex in trivia mode, take the first token.
        let v = toks(&format!("{s} x"));
        assert_eq!(v[0], (k, s.to_owned()), "{s}");
        assert_eq!(v[1].1, "x", "{s}");
    }
}

#[test]
fn operator_sequences_split_greedily() {
    assert_eq!(toks("a**-b").iter().map(|t| t.1.as_str()).collect::<Vec<_>>(), ["a", "**", "-", "b", ""]);
    assert_eq!(tkinds("a<<=b>>c\n"), [K::Name, K::LeftShiftEqual, K::Name, K::RightShift, K::Name, K::Newline]);
    assert_eq!(tkinds("....\n"), [K::Ellipsis, K::Dot, K::Newline]);
    assert_eq!(tkinds("..\n"), [K::Dot, K::Dot, K::Newline]);
    assert_eq!(tkinds("a->b\n"), [K::Name, K::RArrow, K::Name, K::Newline]);
    assert_eq!(tkinds("a<>b\n"), [K::Name, K::Less, K::Greater, K::Name, K::Newline]);
    assert_eq!(tkinds("x:=1\n"), [K::Name, K::ColonEqual, K::Int, K::Newline]);
    assert_eq!(tkinds("a!b\n"), [K::Name, K::Exclamation, K::Name, K::Newline]);
}

#[test]
fn unknown_bytes_are_error_tokens() {
    for s in ["$", "?", "`", "\0", "\x01", "\x7f"] {
        let v = toks(&format!("a {s} b\n"));
        assert_eq!(v[1], (K::ErrorUnknownChar, s.to_owned()), "{s:?}");
        assert_eq!(v[2], (K::Name, "b".to_owned()));
    }
}

// ---------------------------------------------------------------------------
// Logical lines: NEWLINE vs NL, comments, brackets, continuation.

#[test]
fn empty_and_blank_inputs() {
    for s in ["", "\n", "\n\n\n", "   ", "  \n\t\n", "# c", "# c\n", "\x0c\n", "\u{feff}", "\u{feff}\n"] {
        assert!(kinds(s).is_empty(), "{s:?}: {:?}", kinds(s));
        check_invariants(s.as_bytes());
    }
    assert_eq!(Lexer::new(b"").next_token(), Token { start: 0, len: 0, kind: K::EndOfFile });
}

#[test]
fn newline_vs_nl() {
    let src = "x = 1\n\n# comment\n  \ny\n";
    assert_eq!(
        toks(src),
        tt(&[
            (K::Name, "x"), (K::Equal, "="), (K::Int, "1"), (K::Newline, "\n"),
            (K::NonLogicalNewline, "\n"),
            (K::Comment, "# comment"), (K::NonLogicalNewline, "\n"),
            (K::NonLogicalNewline, "\n"),
            (K::Name, "y"), (K::Newline, "\n"),
        ])
    );
    // Parser mode drops trivia.
    assert_eq!(kinds(src), [K::Name, K::Equal, K::Int, K::Newline, K::Name, K::Newline]);
}

#[test]
fn trailing_comment_on_code_line() {
    assert_eq!(toks("x  # note\n"), tt(&[(K::Name, "x"), (K::Comment, "# note"), (K::Newline, "\n")]));
}

#[test]
fn final_newline_synthesised_at_eof() {
    let v = Lexer::new(b"x = 1").tokenize().0;
    assert_eq!(v.iter().map(|t| t.kind).collect::<Vec<_>>(), [K::Name, K::Equal, K::Int, K::Newline, K::EndOfFile]);
    assert_eq!((v[3].start, v[3].len), (5, 0));
    // Comment-only last line without terminator: no NEWLINE.
    assert_eq!(tkinds("x\n# c"), [K::Name, K::Newline, K::Comment]);
}

#[test]
fn brackets_join_lines() {
    let src = "x = (1,\n# c\n     2)\ny\n";
    assert_eq!(
        tkinds(src),
        [
            K::Name, K::Equal, K::LPar, K::Int, K::Comma, K::NonLogicalNewline, K::Comment,
            K::NonLogicalNewline, K::Int, K::RPar, K::Newline, K::Name, K::Newline,
        ]
    );
    // Indentation inside brackets is ignored.
    assert_eq!(kinds("f(\n        a,\n  b)\n"), [K::Name, K::LPar, K::Name, K::Comma, K::Name, K::RPar, K::Newline]);
    // Mixed nesting.
    assert_eq!(kinds("[{(\n)}\n]\n"), [K::LSqb, K::LBrace, K::LPar, K::RPar, K::RBrace, K::RSqb, K::Newline]);
}

#[test]
fn unbalanced_closers_do_not_underflow() {
    assert_eq!(kinds(")\nx\n"), [K::RPar, K::Newline, K::Name, K::Newline]);
    let mut lx = Lexer::new(b"]]}x");
    for _ in 0..3 {
        lx.next_token();
        assert_eq!(lx.bracket_depth(), 0);
    }
    check_invariants(b")]}\n  )\n");
}

#[test]
fn unclosed_bracket_at_eof() {
    let v = kinds("f(a,\n  b\n");
    assert_eq!(v, [K::Name, K::LPar, K::Name, K::Comma, K::Name, K::Newline]);
    check_invariants(b"def f(:\n    x = [\n");
}

#[test]
fn backslash_continuation() {
    assert_eq!(kinds("x = 1 \\\n  + 2\n"), [K::Name, K::Equal, K::Int, K::Plus, K::Int, K::Newline]);
    // No INDENT for the continued line, even at deeper indentation.
    assert_eq!(kinds("if a and \\\n        b:\n    pass\n")[..6], [K::If, K::Name, K::And, K::Name, K::Colon, K::Newline]);
    // CRLF continuation.
    assert_eq!(kinds("x = \\\r\n  1\r\n"), [K::Name, K::Equal, K::Int, K::Newline]);
}

#[test]
fn bad_continuations() {
    assert_eq!(toks("a \\ b\n")[1], (K::ErrorBadContinuation, "\\".to_owned()));
    assert_eq!(toks("a \\ b\n")[2], (K::Name, "b".to_owned()));
    // Continuation into EOF.
    assert_eq!(tkinds("x = \\\n"), [K::Name, K::Equal, K::ErrorBadContinuation, K::Newline]);
    assert_eq!(tkinds("x = \\"), [K::Name, K::Equal, K::ErrorBadContinuation, K::Newline]);
    check_invariants(b"\\");
    check_invariants(b"\\\n");
    check_invariants(b"  \\\n\\\n x");
}

#[test]
fn crlf_and_lone_cr_line_endings() {
    assert_eq!(
        toks("if x:\r\n    y\r\n\r\nz\r\n"),
        tt(&[
            (K::If, "if"), (K::Name, "x"), (K::Colon, ":"), (K::Newline, "\r\n"),
            (K::Indent, "    "), (K::Name, "y"), (K::Newline, "\r\n"),
            (K::NonLogicalNewline, "\r\n"),
            (K::Dedent, ""), (K::Name, "z"), (K::Newline, "\r\n"),
        ])
    );
    assert_eq!(kinds("a\rb\r"), [K::Name, K::Newline, K::Name, K::Newline]);
    assert_eq!(toks("# c\r\nx")[0], (K::Comment, "# c".to_owned()));
}

#[test]
fn bom_is_skipped() {
    let v = Lexer::new("\u{feff}x = 1\n".as_bytes()).tokenize().0;
    assert_eq!(v[0], Token { start: 3, len: 1, kind: K::Name });
    // A BOM anywhere else is identifier bytes.
    assert_eq!(toks("x\u{feff}\n")[0].0, K::Name);
    // An indented first line after a BOM still gets an INDENT.
    assert_eq!(kinds("\u{feff}  x\n")[0], K::Indent);
}

// ---------------------------------------------------------------------------
// Plain strings.

#[test]
fn all_string_prefixes() {
    for p in ["", "r", "u", "b", "R", "U", "B", "br", "rb", "Br", "bR", "RB", "rB", "Rb"] {
        for q in ["'x'", "\"x\"", "'''x'''", "\"\"\"x\"\"\""] {
            let s = format!("{p}{q}");
            assert_eq!(one(&s), (K::String, s.clone()), "{s}");
        }
    }
    // Not prefixes: the word is a name followed by a string.
    for p in ["ur", "bu", "ub", "rr", "bb", "fb", "tb", "ft", "abc"] {
        let s = format!("{p}'x'");
        assert_eq!(tkinds(&s), [K::Name, K::String, K::Newline], "{s}");
    }
}

#[test]
fn string_escapes() {
    assert_eq!(one(r#""a\"b""#), (K::String, r#""a\"b""#.to_owned()));
    assert_eq!(one(r"'a\'b'"), (K::String, r"'a\'b'".to_owned()));
    assert_eq!(one(r"'a\\'"), (K::String, r"'a\\'".to_owned()));
    // Raw strings still cannot end in an odd backslash before the quote.
    assert_eq!(one(r"r'a\'b'"), (K::String, r"r'a\'b'".to_owned()));
    assert_eq!(one(r"r'\\'"), (K::String, r"r'\\'".to_owned()));
    // Backslash-newline continues a single-quoted string (LF and CRLF).
    assert_eq!(toks("'a\\\nb'\n")[0], (K::String, "'a\\\nb'".to_owned()));
    assert_eq!(toks("'a\\\r\nb'\r\n")[0], (K::String, "'a\\\r\nb'".to_owned()));
    // The other quote kind needs no escape.
    assert_eq!(one(r#"'say "hi"'"#).0, K::String);
}

#[test]
fn triple_quoted_strings() {
    let src = "x = '''a\n'b'' \\''' c\n'''\ny\n";
    let v = toks(src);
    assert_eq!(v[2], (K::String, "'''a\n'b'' \\''' c\n'''".to_owned()));
    assert_eq!(v[3].0, K::Newline);
    assert_eq!(v[4], (K::Name, "y".to_owned()));
    // `""""a""""` is the string `"""` + `"a` + `"""`, then a lone `"` (unterminated).
    let v = toks("\"\"\"\"a\"\"\"\"\n");
    assert_eq!(v[0], (K::String, "\"\"\"\"a\"\"\"".to_owned()));
    assert_eq!(v[1], (K::ErrorUnterminatedString, "\"".to_owned()));
    // Empty strings.
    assert_eq!(tkinds("''\"\"''''''\n"), [K::String, K::String, K::String, K::Newline]);
    // A triple-quoted string spanning lines produces no NEWLINE/INDENT inside.
    let v = kinds("if x:\n    s = \"\"\"\nfoo\n  bar\n\"\"\"\n    y\n");
    assert_eq!(v.iter().filter(|k| **k == K::Indent).count(), 1);
    assert_eq!(v.iter().filter(|k| **k == K::Newline).count(), 3);
}

#[test]
fn unterminated_strings() {
    // Single-quoted: error up to the line end, lexing resumes on the next line.
    assert_eq!(
        toks("x = 'abc\ny\n"),
        tt(&[
            (K::Name, "x"), (K::Equal, "="), (K::ErrorUnterminatedString, "'abc"), (K::Newline, "\n"),
            (K::Name, "y"), (K::Newline, "\n"),
        ])
    );
    // At EOF, with and without a trailing backslash.
    assert_eq!(toks("'abc")[0], (K::ErrorUnterminatedString, "'abc".to_owned()));
    assert_eq!(toks("'abc\\")[0], (K::ErrorUnterminatedString, "'abc\\".to_owned()));
    assert_eq!(toks("b'\\\n")[0], (K::ErrorUnterminatedString, "b'\\\n".to_owned()));
    // Triple-quoted: error spans to EOF.
    assert_eq!(toks("x = '''abc\ndef")[2], (K::ErrorUnterminatedTripleString, "'''abc\ndef".to_owned()));
    assert_eq!(toks("'''\\")[0], (K::ErrorUnterminatedTripleString, "'''\\".to_owned()));
    assert_eq!(toks("''''")[0], (K::ErrorUnterminatedTripleString, "''''".to_owned()));
    for s in ["'", "\"", "'''", "r'", "b\"\"\"", "'\\", "'''\\", "\"\"\"\"\""] {
        check_invariants(s.as_bytes());
    }
}

#[test]
fn strings_with_non_utf8_bytes() {
    let v = toks_bytes(b"b'\xff\xfe' '\x80'\n");
    assert_eq!(v.iter().map(|t| t.0).collect::<Vec<_>>(), [K::String, K::String, K::Newline]);
}

// ---------------------------------------------------------------------------
// f-strings and t-strings (PEP 701 / PEP 750). Expected streams match CPython 3.14
// `tokenize` except that CPython splits literal text at `{{`/`}}`.

#[test]
fn fstring_prefixes() {
    for p in ["f", "F", "rf", "fr", "Rf", "fR", "RF", "FR", "rF", "Fr"] {
        for q in ["'", "\"", "'''", "\"\"\""] {
            let s = format!("{p}{q}a{{x}}{q}");
            assert_eq!(
                toks(&s),
                tt(&[(K::FStringStart, &format!("{p}{q}")), (K::FStringMiddle, "a"), (K::LBrace, "{"), (K::Name, "x"), (K::RBrace, "}"), (K::FStringEnd, q), (K::Newline, "")]),
                "{s}"
            );
        }
    }
    for p in ["t", "T", "rt", "tr", "Rt", "tR", "RT", "TR"] {
        let s = format!("{p}'a{{x}}'");
        assert_eq!(
            tkinds(&s),
            [K::TStringStart, K::TStringMiddle, K::LBrace, K::Name, K::RBrace, K::TStringEnd, K::Newline],
            "{s}"
        );
    }
}

#[test]
fn fstring_literal_braces_and_debug() {
    // `{{`/`}}` stay inside one middle token (raw span).
    assert_eq!(
        toks("f\"a{{b}}c{x=}\"\n"),
        tt(&[
            (K::FStringStart, "f\""), (K::FStringMiddle, "a{{b}}c"), (K::LBrace, "{"), (K::Name, "x"),
            (K::Equal, "="), (K::RBrace, "}"), (K::FStringEnd, "\""), (K::Newline, "\n"),
        ])
    );
    assert_eq!(tkinds("f'{{}}'\n"), [K::FStringStart, K::FStringMiddle, K::FStringEnd, K::Newline]);
    assert_eq!(tkinds("f''\n"), [K::FStringStart, K::FStringEnd, K::Newline]);
    // `!=` inside a field is an operator, `!r` a conversion.
    assert_eq!(
        tkinds("f'{a!=b}{c!r}'\n"),
        [
            K::FStringStart, K::LBrace, K::Name, K::NotEqual, K::Name, K::RBrace, K::LBrace, K::Name,
            K::Exclamation, K::Name, K::RBrace, K::FStringEnd, K::Newline,
        ]
    );
}

#[test]
fn fstring_format_specs() {
    assert_eq!(
        toks("f\"{x:{y}.{z}f}\"\n"),
        tt(&[
            (K::FStringStart, "f\""), (K::LBrace, "{"), (K::Name, "x"), (K::Colon, ":"),
            (K::LBrace, "{"), (K::Name, "y"), (K::RBrace, "}"), (K::FStringMiddle, "."),
            (K::LBrace, "{"), (K::Name, "z"), (K::RBrace, "}"), (K::FStringMiddle, "f"),
            (K::RBrace, "}"), (K::FStringEnd, "\""), (K::Newline, "\n"),
        ])
    );
    assert_eq!(
        toks("f'{x!r:>{w}}'\n"),
        tt(&[
            (K::FStringStart, "f'"), (K::LBrace, "{"), (K::Name, "x"), (K::Exclamation, "!"), (K::Name, "r"),
            (K::Colon, ":"), (K::FStringMiddle, ">"), (K::LBrace, "{"), (K::Name, "w"), (K::RBrace, "}"),
            (K::RBrace, "}"), (K::FStringEnd, "'"), (K::Newline, "\n"),
        ])
    );
    // A top-level `:=` in a field is a spec starting with `=`; inside parens it is a walrus.
    assert_eq!(toks("f'{x:=10}'\n")[3..5], tt(&[(K::Colon, ":"), (K::FStringMiddle, "=10")]));
    assert_eq!(tkinds("f'{(x:=10)}'\n")[2..6], [K::LPar, K::Name, K::ColonEqual, K::Int]);
    // Colons nested in brackets, lambdas in parens and dict displays are not specs.
    assert_eq!(
        tkinds("f'{a[1:2]}{(lambda: 1)()}'\n").iter().filter(|k| **k == K::FStringMiddle).count(),
        0
    );
    assert_eq!(tkinds("f'{ {1: 2}[1] }'\n")[2..7], [K::LBrace, K::Int, K::Colon, K::Int, K::RBrace]);
    // Empty spec and escapes in a spec.
    assert_eq!(tkinds("f'{x:}'\n"), [K::FStringStart, K::LBrace, K::Name, K::Colon, K::RBrace, K::FStringEnd, K::Newline]);
    assert_eq!(toks("f'{x:\\n}'\n")[4], (K::FStringMiddle, "\\n".to_owned()));
    // A spec on a nested field inside a spec.
    assert_eq!(
        tkinds("f'{x:{y:>3}}'\n"),
        [
            K::FStringStart, K::LBrace, K::Name, K::Colon, K::LBrace, K::Name, K::Colon, K::FStringMiddle,
            K::RBrace, K::RBrace, K::FStringEnd, K::Newline,
        ]
    );
}

#[test]
fn nested_fstrings_with_same_quotes() {
    assert_eq!(
        toks("f\"{f\"{x}\"}\"\n"),
        tt(&[
            (K::FStringStart, "f\""), (K::LBrace, "{"), (K::FStringStart, "f\""), (K::LBrace, "{"),
            (K::Name, "x"), (K::RBrace, "}"), (K::FStringEnd, "\""), (K::RBrace, "}"),
            (K::FStringEnd, "\""), (K::Newline, "\n"),
        ])
    );
    // Plain strings with the same quote inside a field, and deep nesting.
    assert_eq!(tkinds("f'{'a'}'\n"), [K::FStringStart, K::LBrace, K::String, K::RBrace, K::FStringEnd, K::Newline]);
    let deep = "f'{f'{f'{f'{f'{f'{1}'}'}'}'}'}'\n";
    let v = tkinds(deep);
    assert_eq!(v.iter().filter(|k| **k == K::FStringStart).count(), 6);
    assert_eq!(v.iter().filter(|k| **k == K::FStringEnd).count(), 6);
    assert!(!v.iter().any(|k| k.is_error()));
    // f-string inside a t-string and vice versa.
    assert_eq!(
        tkinds("t'{f\"{x}\"}'\n"),
        [
            K::TStringStart, K::LBrace, K::FStringStart, K::LBrace, K::Name, K::RBrace, K::FStringEnd,
            K::RBrace, K::TStringEnd, K::Newline,
        ]
    );
}

#[test]
fn fstring_backslashes() {
    // `\N{...}` is a named escape in non-raw strings, a field in raw ones.
    assert_eq!(toks("f\"\\N{DASH}{x}\"\n")[1], (K::FStringMiddle, "\\N{DASH}".to_owned()));
    assert_eq!(toks("rf\"\\N{x}\"\n")[1..3], tt(&[(K::FStringMiddle, "\\N"), (K::LBrace, "{")]));
    // Same inside a format spec.
    assert_eq!(toks("f'{x:\\N{BULLET}}'\n")[4..6], tt(&[(K::FStringMiddle, "\\N{BULLET}"), (K::RBrace, "}")]));
    assert_eq!(toks("rf'{x:\\N{y}}'\n")[4..6], tt(&[(K::FStringMiddle, "\\N"), (K::LBrace, "{")]));
    // Escaped quote does not close; backslash before a brace keeps the brace.
    assert_eq!(toks("f'a\\'b'\n")[1], (K::FStringMiddle, "a\\'b".to_owned()));
    assert_eq!(toks("f'\\{x}'\n")[1..3], tt(&[(K::FStringMiddle, "\\"), (K::LBrace, "{")]));
    // Backslashes and strings with escapes inside the expression part (allowed since 3.12).
    assert_eq!(tkinds("f'{\"\\n\".join(a)}'\n")[2], K::String);
    // Raw f-string: `\'` still does not close it.
    assert_eq!(toks("rf'\\'{x}'\n")[1], (K::FStringMiddle, "\\'".to_owned()));
}

#[test]
fn multiline_fields_and_comments() {
    assert_eq!(
        toks("f\"\"\"{\nx # c\n}\"\"\"\n"),
        tt(&[
            (K::FStringStart, "f\"\"\""), (K::LBrace, "{"), (K::NonLogicalNewline, "\n"), (K::Name, "x"),
            (K::Comment, "# c"), (K::NonLogicalNewline, "\n"), (K::RBrace, "}"), (K::FStringEnd, "\"\"\""),
            (K::Newline, "\n"),
        ])
    );
    // A newline inside the field of a single-quoted f-string is allowed (NL).
    assert_eq!(
        tkinds("f\"{x\n}\"\n"),
        [K::FStringStart, K::LBrace, K::Name, K::NonLogicalNewline, K::RBrace, K::FStringEnd, K::Newline]
    );
    // Triple-quoted literal text spans lines in one middle token.
    assert_eq!(toks("f'''a\nb{x}'''\n")[1], (K::FStringMiddle, "a\nb".to_owned()));
}

#[test]
fn unterminated_fstrings() {
    // At EOF, and at a newline in a single-quoted f-string: lexing resumes on the next line.
    assert_eq!(
        toks("f\"abc"),
        tt(&[(K::FStringStart, "f\""), (K::FStringMiddle, "abc"), (K::ErrorUnterminatedFString, ""), (K::Newline, "")])
    );
    assert_eq!(
        toks("f\"abc\nx = 1\n"),
        tt(&[
            (K::FStringStart, "f\""), (K::FStringMiddle, "abc"), (K::ErrorUnterminatedFString, ""), (K::Newline, "\n"),
            (K::Name, "x"), (K::Equal, "="), (K::Int, "1"), (K::Newline, "\n"),
        ])
    );
    // EOF inside a replacement field (single- and triple-quoted).
    assert_eq!(tkinds("f\"{"), [K::FStringStart, K::LBrace, K::ErrorUnterminatedFString, K::Newline]);
    assert_eq!(
        tkinds("f'''{x\n"),
        [K::FStringStart, K::LBrace, K::Name, K::NonLogicalNewline, K::ErrorUnterminatedFString, K::Newline]
    );
    assert_eq!(tkinds("t'''abc\n"), [K::TStringStart, K::TStringMiddle, K::ErrorUnterminatedFString, K::Newline]);
    for s in ["f'", "f'''", "f'{", "f'{x:", "f'{x:{", "f'{x!", "rf'\\", "f'\\", "f'\\N{", "f'''{\n", "t'{x"] {
        check_invariants(s.as_bytes());
    }
}

#[test]
fn fstring_structure_errors() {
    // Missing `}` before the closing quote: zero-length error, then the string ends there.
    assert_eq!(
        toks("f\"{x\"\ny\n"),
        tt(&[
            (K::FStringStart, "f\""), (K::LBrace, "{"), (K::Name, "x"), (K::ErrorFString, ""), (K::FStringEnd, "\""),
            (K::Newline, "\n"), (K::Name, "y"), (K::Newline, "\n"),
        ])
    );
    assert_eq!(
        tkinds("f\"{x:abc\"\ny\n"),
        [
            K::FStringStart, K::LBrace, K::Name, K::Colon, K::FStringMiddle, K::ErrorFString, K::FStringEnd,
            K::Newline, K::Name, K::Newline,
        ]
    );
    // Single `}` in literal text.
    assert_eq!(toks("f\"{x}}\"\n")[4], (K::ErrorFString, "}".to_owned()));
    assert_eq!(tkinds("f'}'\n"), [K::FStringStart, K::ErrorFString, K::FStringEnd, K::Newline]);
    // Newline in a single-quoted format spec.
    assert_eq!(
        tkinds("f\"{x:abc\ny\n"),
        [
            K::FStringStart, K::LBrace, K::Name, K::Colon, K::FStringMiddle, K::ErrorFString,
            K::ErrorUnterminatedFString, K::Newline, K::Name, K::Newline,
        ]
    );
    // `}` with an unclosed `(` inside the field closes the field (recovery).
    assert_eq!(
        tkinds("f\"{x(}\"\ny\n"),
        [K::FStringStart, K::LBrace, K::Name, K::LPar, K::RBrace, K::FStringEnd, K::Newline, K::Name, K::Newline]
    );
    // A stray closer inside a field cannot pop the field's own `{`.
    assert_eq!(tkinds("f'{x)}'\n"), [K::FStringStart, K::LBrace, K::Name, K::RPar, K::RBrace, K::FStringEnd, K::Newline]);
    // Unterminated string of another kind inside a field abandons the f-string at the line end.
    assert_eq!(
        tkinds("f'{\"a}'\ny\n"),
        [K::FStringStart, K::LBrace, K::ErrorUnterminatedString, K::Newline, K::Name, K::Newline]
    );
    assert_eq!(
        tkinds("f'{x!r'\ny\n"),
        [K::FStringStart, K::LBrace, K::Name, K::Exclamation, K::ErrorUnterminatedString, K::Newline, K::Name, K::Newline]
    );
}

#[test]
fn fstring_nesting_limit() {
    // Nesting deeper than the mode stack degrades to error tokens, never a panic.
    let mut s = String::new();
    for _ in 0..40 {
        s.push_str("f'{");
    }
    s.push('1');
    for _ in 0..40 {
        s.push_str("}'");
    }
    s.push('\n');
    let v = tkinds(&s);
    assert!(v.contains(&K::ErrorFString));
    assert_eq!(v.last(), Some(&K::Newline));
    check_invariants(s.as_bytes());
    // Many sequential (not nested) f-strings are fine.
    let flat = "f'{a}' ".repeat(100) + "\n";
    assert!(!tkinds(&flat).iter().any(|k| k.is_error()));
}

// ---------------------------------------------------------------------------
// Indentation.

#[test]
fn indent_and_dedent() {
    assert_eq!(
        toks("def f():\n    if x:\n        y\n    z\nw\n"),
        tt(&[
            (K::Def, "def"), (K::Name, "f"), (K::LPar, "("), (K::RPar, ")"), (K::Colon, ":"), (K::Newline, "\n"),
            (K::Indent, "    "), (K::If, "if"), (K::Name, "x"), (K::Colon, ":"), (K::Newline, "\n"),
            (K::Indent, "        "), (K::Name, "y"), (K::Newline, "\n"),
            (K::Dedent, ""), (K::Name, "z"), (K::Newline, "\n"),
            (K::Dedent, ""), (K::Name, "w"), (K::Newline, "\n"),
        ])
    );
    // Several dedents at once, and outstanding dedents at EOF (with and without a final newline).
    assert_eq!(
        kinds("if a:\n  if b:\n    c\nd\n"),
        [
            K::If, K::Name, K::Colon, K::Newline, K::Indent, K::If, K::Name, K::Colon, K::Newline, K::Indent,
            K::Name, K::Newline, K::Dedent, K::Dedent, K::Name, K::Newline,
        ]
    );
    assert_eq!(kinds("if a:\n  if b:\n    c\n"), kinds("if a:\n  if b:\n    c"));
    assert_eq!(kinds("if a:\n  b\n\n   \n"), [K::If, K::Name, K::Colon, K::Newline, K::Indent, K::Name, K::Newline, K::Dedent]);
    // Dedent tokens sit at the first token of the line.
    let v = Lexer::new(b"if a:\n  b\nc\n").tokenize().0;
    let d = v.iter().find(|t| t.kind == K::Dedent).unwrap();
    assert_eq!((d.start, d.len), (10, 0));
    // An indented first line is an INDENT (the parser reports it).
    assert_eq!(kinds(" x\n"), [K::Indent, K::Name, K::Newline, K::Dedent]);
}

#[test]
fn blank_and_comment_lines_do_not_affect_indentation() {
    assert_eq!(
        tkinds("if a:\n    b\n  # c\n        # d\n\n \t \n    e\n"),
        [
            K::If, K::Name, K::Colon, K::Newline, K::Indent, K::Name, K::Newline, K::Comment, K::NonLogicalNewline,
            K::Comment, K::NonLogicalNewline, K::NonLogicalNewline, K::NonLogicalNewline, K::Name, K::Newline,
            K::Dedent,
        ]
    );
}

#[test]
fn tabs_and_spaces() {
    // Tabs advance to the next multiple of 8; consistent tab use is fine.
    assert_eq!(kinds("if a:\n\tb\n\tif c:\n\t\td\n").iter().filter(|k| **k == K::Indent).count(), 2);
    // A tab and 8 spaces are the same column but inconsistent (TabError) both ways.
    for s in ["if a:\n\tb\n        c\n", "if a:\n        b\n\tc\n"] {
        assert_eq!(kinds(s), [K::If, K::Name, K::Colon, K::Newline, K::Indent, K::Name, K::Newline, K::ErrorTabs, K::Name, K::Newline, K::Dedent], "{s:?}");
    }
    // Deeper by tab-8 but not by tab-1.
    let v = kinds("if a:\n  \tb\n\t  c\n");
    assert_eq!(v[7..9], [K::Indent, K::ErrorTabs]);
    // Dedent to a level that matches by tab-8 only.
    assert!(kinds("if a:\n\tif b:\n\t\tc\n        d\n").contains(&K::ErrorTabs));
    // Spaces then tab that lands on the same column as a tab: `  \t` == `\t` by tab-8, but not by tab-1.
    assert!(kinds("if a:\n\tb\n  \tc\n").contains(&K::ErrorTabs));
    // Form feed resets the column.
    assert_eq!(kinds("if a:\n    b\n\x0c    c\n").iter().filter(|k| k.is_error()).count(), 0);
}

#[test]
fn dedent_to_unknown_level() {
    // The line joins the enclosing block; following lines at the same column are not an INDENT.
    assert_eq!(
        kinds("if a:\n    b\n  c\n  d\ne\n"),
        [
            K::If, K::Name, K::Colon, K::Newline, K::Indent, K::Name, K::Newline, K::Dedent, K::ErrorDedent,
            K::Name, K::Newline, K::Name, K::Newline, K::Name, K::Newline,
        ]
    );
    // Indenting past the unknown level still balances.
    let s = "if a:\n    b\n  c\n      d\n";
    assert_eq!(kinds(s).iter().filter(|k| **k == K::Indent).count(), 2);
    check_invariants(s.as_bytes());
    // Form feed then fewer spaces than the block.
    assert!(kinds("if a:\n    b\n\x0c  c\n").contains(&K::ErrorDedent));
    for s in ["if a:\n  b\n c\nd\n", "  a\n b\nc\n", "    a\n  b\n   c\n d\n", "\tif x:\n  y\n"] {
        check_invariants(s.as_bytes());
    }
}

#[test]
fn indentation_too_deep() {
    let mut s = String::new();
    for i in 0..(MAX_INDENT + 5) {
        s.push_str(&" ".repeat(i));
        s.push_str("if x:\n");
    }
    let v = kinds(&s);
    assert!(v.contains(&K::ErrorTooDeep));
    assert_eq!(v.iter().filter(|k| **k == K::Indent).count(), MAX_INDENT - 1);
    check_invariants(s.as_bytes());
}

// ---------------------------------------------------------------------------
// Suppression comments.

/// (directive text from `#`, codes) for every suppression in `src`.
fn sups(src: &str) -> Vec<(String, Vec<String>, bool)> {
    let (_, s) = Lexer::new(src.as_bytes()).tokenize();
    s.iter()
        .map(|x| {
            let codes = x.codes(src.as_bytes()).map(|c| String::from_utf8_lossy(c).into_owned()).collect();
            (src[x.start as usize..].lines().next().unwrap().to_owned(), codes, x.all)
        })
        .collect()
}

#[test]
fn suppression_comments() {
    let v = sups("x = 1  # pymapper: ignore[PM101, PM202]\n");
    assert_eq!(v, [("# pymapper: ignore[PM101, PM202]".to_owned(), vec!["PM101".to_owned(), "PM202".to_owned()], false)]);
    // Compact spacing, empty entries, bare ignore (all codes).
    assert_eq!(sups("#pymapper:ignore[PM1,,PM2 ,]\n")[0].1, ["PM1", "PM2"]);
    assert!(sups("y  # pymapper: ignore\n")[0].2);
    assert!(sups("y  # pymapper: ignore  # why\n")[0].2);
    assert_eq!(sups("y  #\tpymapper:\tignore[PM9]\n")[0].1, ["PM9"]);
    // After another tool's directive in the same comment.
    assert_eq!(sups("z  # noqa: E501  # pymapper: ignore[PM3]\n")[0].1, ["PM3"]);
    // Recorded in comment-only lines, inside brackets and in f-string fields; and in trivia mode.
    assert_eq!(sups("# pymapper: ignore[A]\nf(\n  1,  # pymapper: ignore[B]\n)\n").len(), 2);
    assert_eq!(sups("f'''{\nx  # pymapper: ignore[C]\n}'''\n")[0].1, ["C"]);
    assert_eq!(Lexer::with_trivia(b"#pymapper: ignore[X]\n").tokenize().1.len(), 1);
    // Not directives.
    for s in [
        "# pymapper: ignored\n", "# pymapper: ignore-file\n", "# pymapper ignore[X]\n", "# PYMAPPER: ignore[X]\n",
        "# pymapper: ignore[X\n", "s = '# pymapper: ignore[X]'\n", "# pymapper:\n", "# mapper: ignore\n",
    ] {
        assert!(sups(s).is_empty(), "{s:?}");
    }
    // Offsets point into the source (BOM, CRLF).
    let src = "\u{feff}a # pymapper: ignore[Q]\r\n";
    let (_, s) = Lexer::new(src.as_bytes()).tokenize();
    assert_eq!(s[0].start, 5);
    assert_eq!(s[0].codes(src.as_bytes()).collect::<Vec<_>>(), [b"Q"]);
}

// ---------------------------------------------------------------------------
// Checkpoint / restore.

#[test]
fn checkpoint_restore_replays_within_logical_lines() {
    let src = "x = f'{a:{b}>{c!r}}' + t\"{f'{d}'}\" + [1,\n  2] # c\nif y:\n    z(f'''{\n e}''')\n";
    let all = Lexer::new(src.as_bytes()).tokenize().0;
    // From every token boundary, look ahead to the end of the logical line, restore, replay.
    let boundary = |k: TokenKind| matches!(k, K::Newline | K::Indent | K::Dedent | K::EndOfFile);
    for i in 0..all.len() {
        // Tokens up to and including the next line boundary.
        let n = all[i..].iter().position(|t| boundary(t.kind)).map_or(all.len() - i, |j| j + 1);
        let mut lx = Lexer::new(src.as_bytes());
        for _ in 0..i {
            lx.next_token();
        }
        let cp = lx.checkpoint();
        // Read ahead up to (not including) the boundary: indentation is unchanged, so restore is valid.
        for _ in 0..n - 1 {
            lx.next_token();
        }
        lx.restore(cp);
        for t in &all[i..i + n] {
            assert_eq!(lx.next_token(), *t, "replay from token {i}");
        }
    }
}

#[test]
fn restore_drops_and_rerecords_suppressions() {
    let src = b"f(a,  # pymapper: ignore[A]\n  b)\n";
    let mut lx = Lexer::new(src);
    lx.next_token();
    let cp = lx.checkpoint();
    for _ in 0..4 {
        lx.next_token();
    }
    assert_eq!(lx.suppressions().len(), 1);
    lx.restore(cp);
    assert_eq!(lx.suppressions().len(), 0);
    while lx.next_token().kind != K::EndOfFile {}
    assert_eq!(lx.take_suppressions().len(), 1);
}

#[test]
fn checkpoint_is_small() {
    // Cheap to copy: position, bracket state and the f-string mode stack only.
    assert!(std::mem::size_of::<Checkpoint>() <= 320, "{}", std::mem::size_of::<Checkpoint>());
}

// ---------------------------------------------------------------------------
// Robustness on arbitrary bytes.

/// xorshift64*: deterministic, dependency-free.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

/// Fragments that exercise every lexer mode when spliced together randomly.
const FRAGMENTS: &[&[u8]] = &[
    b"f'", b"f\"", b"f'''", b"f\"\"\"", b"rf'", b"t'", b"b'", b"'", b"\"", b"'''", b"\"\"\"", b"{", b"}", b"{{",
    b"}}", b"(", b")", b"[", b"]", b":", b"!r", b"=", b"\\", b"\\N{", b"\n", b"\r\n", b"\r", b"  ", b"\t", b"\x0c",
    b"#", b"# pymapper: ignore[PM1]", b"x", b"if", b"0x", b"1_", b"1.5e", b".", b"...", b"\xef\xbb\xbf", b"\xff",
    b"\0", b"$", b"->", b"**=", b"0", b"j", b"_", b"async", b"\\\n",
];

#[test]
fn fuzz_spliced_fragments() {
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let mut buf = Vec::new();
    for _ in 0..20_000 {
        buf.clear();
        for _ in 0..rng.below(40) {
            buf.extend_from_slice(FRAGMENTS[rng.below(FRAGMENTS.len())]);
        }
        check_invariants(&buf);
    }
}

#[test]
fn fuzz_random_bytes() {
    let mut rng = Rng(0x0123_4567_89AB_CDEF);
    let mut buf = Vec::new();
    for _ in 0..10_000 {
        buf.clear();
        for _ in 0..rng.below(64) {
            // Bias towards bytes with meaning to the lexer.
            let b = if rng.below(2) == 0 { b"'\"{}()[]\\\n\r\t #fr:!=."[rng.below(20)] } else { rng.next() as u8 };
            buf.push(b);
        }
        check_invariants(&buf);
    }
}

#[test]
fn every_prefix_of_a_real_file() {
    // Truncation at every byte exercises all EOF paths (inside strings, fields, specs, brackets).
    let src = b"import os\nclass A(B):\n    def f(self, x=1):\n        '''doc'''\n        return f\"{x!r:>{w}} {'a'}\" + t'{y}' \\\n            + rb'\\x00'  # pymapper: ignore[PM1]\n";
    for end in 0..=src.len() {
        check_invariants(&src[..end]);
    }
}

#[test]
fn corpus_files_lex_without_errors() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/corpus");
    let mut stack = vec![root];
    let mut files = 0;
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else if p.extension().is_some_and(|x| x == "py") {
                let src = std::fs::read(&p).unwrap();
                check_invariants(&src);
                let (v, _) = Lexer::new(&src).tokenize();
                let errors: Vec<_> = v.iter().filter(|t| t.kind.is_error()).collect();
                assert!(errors.is_empty(), "{}: {errors:?}", p.display());
                files += 1;
            }
        }
    }
    assert!(files > 0);
}
