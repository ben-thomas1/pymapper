//! Fuzz target registry.
//!
//! A target is a plain `fn(&[u8])` that must never panic, hang, or abort for any input. Add a
//! target by appending an entry to [`TARGETS`]; the first entry is the default.

use std::hint::black_box;

use pymapper_core::extract::{self, Visitor};
use pymapper_core::extract::events::{Block, BlockKind, Def, DefKind, End};
use pymapper_core::lex::{Lexer, Token, TokenKind};
use std::time::Duration;

/// A named fuzz target.
pub struct FuzzTarget {
    pub name: &'static str,
    pub about: &'static str,
    /// Self-test targets have planted bugs; they exist to test the fuzzer itself.
    pub selftest: bool,
    pub run: fn(&[u8]),
}

pub const TARGETS: &[FuzzTarget] = &[
    FuzzTarget {
        name: "extract",
        about: "facts + metrics in one pass: index/span invariants, span vs interner sinks agree",
        selftest: false,
        run: extract,
    },
    FuzzTarget {
        name: "lex",
        about: "lexer: token order/bounds, termination, checkpoint/restore replay, trivia mode agrees",
        selftest: false,
        run: lex,
    },
    FuzzTarget {
        name: "parse",
        about: "pymapper_core::extract parser: event balance, token order and span invariants",
        selftest: false,
        run: parse,
    },
    FuzzTarget {
        name: "demo",
        about: "toy tokenizer with invariant checks (tests the fuzzer itself)",
        selftest: false,
        run: demo,
    },
    FuzzTarget {
        name: "selftest-panic",
        about: "planted panics on a NUL byte and on '{{{{' (tests the fuzzer)",
        selftest: true,
        run: selftest_panic,
    },
    FuzzTarget {
        name: "selftest-hang",
        about: "planted 1.5 s stall on a UTF-8 BOM anywhere in the input (tests the fuzzer)",
        selftest: true,
        run: selftest_hang,
    },
    FuzzTarget {
        name: "selftest-abort",
        about: "planted unbounded recursion (stack overflow kills the process) on a backtick (tests the fuzzer)",
        selftest: true,
        run: selftest_abort,
    },
];

pub fn find(name: &str) -> Option<&'static FuzzTarget> {
    TARGETS.iter().find(|t| t.name == name)
}

/// Parser invariant checker: definition/block/lambda events balance, consumed
/// tokens are in order and inside the source.
struct ParseCheck {
    len: usize,
    defs: u32,
    blocks: Vec<BlockKind>,
    lambdas: u32,
    last_end: u32,
}

impl Visitor for ParseCheck {
    fn token(&mut self, t: Token) {
        assert!(t.end() as usize <= self.len, "token out of bounds");
        assert!(t.start >= self.last_end, "tokens out of order");
        self.last_end = t.end();
    }
    fn def_enter(&mut self, d: &Def<'_>) {
        assert!(d.start <= d.name.start, "def start after its name");
        self.defs += 1;
    }
    fn def_exit(&mut self, _: DefKind, end: End) {
        assert!(end.end as usize <= self.len);
        self.defs = self.defs.checked_sub(1).expect("def_exit without def_enter");
    }
    fn block_enter(&mut self, b: &Block<'_>) {
        self.blocks.push(b.kind);
    }
    fn block_exit(&mut self, kind: BlockKind, _: End, _: bool) {
        assert_eq!(self.blocks.pop(), Some(kind), "unbalanced block events");
    }
    fn lambda_enter(&mut self, _: Token) {
        self.lambdas += 1;
    }
    fn lambda_exit(&mut self, _: u32) {
        self.lambdas = self.lambdas.checked_sub(1).expect("lambda_exit without lambda_enter");
    }
}

fn parse(src: &[u8]) {
    let c = extract::parse(src, ParseCheck { len: src.len(), defs: 0, blocks: Vec::new(), lambdas: 0, last_end: 0 });
    assert_eq!(c.defs, 0, "def_enter without def_exit");
    assert!(c.blocks.is_empty(), "block_enter without block_exit");
    assert_eq!(c.lambdas, 0, "lambda_enter without lambda_exit");
    black_box(c.last_end);
}

/// Most tokens a source of `len` bytes may produce: each line yields at most one NEWLINE and
/// one INDENT (and later the matching DEDENT), each needing bytes of its own, plus the final
/// NEWLINE, DEDENTs and EOF.
fn max_tokens(len: usize) -> usize {
    3 * len + 8
}

#[inline]
fn is_layout(k: TokenKind) -> bool {
    matches!(k, TokenKind::Newline | TokenKind::Indent | TokenKind::Dedent | TokenKind::EndOfFile)
}

/// Lexes `src` to EOF, checking order, bounds and termination; returns the tokens (EOF
/// included) and the lexer.
fn lex_all<'s>(mut lx: Lexer<'s>, len: usize) -> (Vec<Token>, Lexer<'s>) {
    let mut toks = Vec::new();
    let mut last_end = 0u32;
    loop {
        let t = lx.next_token();
        assert!(t.end() as usize <= len, "token out of bounds");
        assert!(t.start >= last_end, "tokens out of order or overlapping");
        last_end = t.end();
        toks.push(t);
        assert!(toks.len() <= max_tokens(len), "lexer does not terminate");
        if t.kind == TokenKind::EndOfFile {
            break;
        }
    }
    for _ in 0..2 {
        assert_eq!(lx.next_token().kind, TokenKind::EndOfFile, "EOF must be sticky");
    }
    (toks, lx)
}

/// Lexer target: order, bounds and termination in both modes; trivia mode minus trivia equals
/// parser mode; a checkpoint taken mid-line replays the stream exactly after restore.
fn lex(src: &[u8]) {
    let (toks, lx) = lex_all(Lexer::new(src), src.len());
    let (trivia, lxt) = lex_all(Lexer::with_trivia(src), src.len());
    let mut code = trivia.iter().filter(|t| !t.kind.is_trivia());
    for t in &toks {
        assert_eq!(code.next(), Some(t), "trivia mode disagrees with parser mode");
        assert!(!t.kind.is_trivia(), "trivia token in parser mode");
    }
    assert_eq!(code.next(), None, "trivia mode has extra tokens");
    assert_eq!(lx.suppressions(), lxt.suppressions(), "suppressions depend on the mode");

    // Checkpoint after a mid-stream non-layout token, run to the end of its logical line,
    // restore, and check the replay and the rest of the stream.
    let Some(k) = (toks.len() / 2..toks.len()).find(|&i| !is_layout(toks[i].kind)) else { return };
    let mut lx = Lexer::new(src);
    for want in &toks[..=k] {
        assert_eq!(lx.next_token(), *want, "lexing is not deterministic");
    }
    let cp = lx.checkpoint();
    let mut n = 0;
    while n < 64 {
        let t = lx.next_token();
        assert_eq!(t, toks[k + 1 + n], "lexing is not deterministic");
        n += 1;
        if is_layout(t.kind) {
            break;
        }
    }
    // Restoring is only allowed while the indent stack is unchanged: no INDENT/DEDENT
    // returned (a NEWLINE is fine, indentation is processed when the next line is lexed).
    if toks[k + 1..k + 1 + n].iter().any(|t| matches!(t.kind, TokenKind::Indent | TokenKind::Dedent | TokenKind::EndOfFile)) {
        return;
    }
    lx.restore(cp);
    for want in &toks[k + 1..] {
        assert_eq!(lx.next_token(), *want, "restore did not replay the stream");
    }
}

/// Facts extraction target: runs facts + metrics over `src` with the span sink, checks every
/// index and span invariant (rendering resolves every name and chain), then extracts again
/// into the same store with an interner and checks both renderings agree.
fn extract(src: &[u8]) {
    use pymapper_core::extract::facts::{self as cf, DefKind as CoreKind, FactStore, NO_PARENT, SpanStrs, render};
    use pymapper_core::extract::{MetricsScratch, MetricsTable, MetricsVisitor};
    use pymapper_core::intern::Interner;

    let mut st = FactStore::new();
    let mut spans = SpanStrs::new();
    let mut scratch = MetricsScratch::new();
    let mut metrics = MetricsTable::new();
    let m = MetricsVisitor::new(src, &mut scratch, &mut metrics);
    let (ff, _) = cf::extract_with(src, &mut st, &mut spans, m);
    let v = st.file(&ff);
    let n = v.def_count();
    assert!(n >= 1, "no <module> row");
    assert_eq!(metrics.len() as u32, n - 1, "one metrics row per def");
    let max_line = ff.lines.max(1);
    let d = &st.defs;
    assert_eq!((CoreKind::from_u8(d.kind[0]), d.parent[0], d.name[0]), (CoreKind::Module, NO_PARENT, None));
    for i in 1..n as usize {
        assert!(d.parent[i] < i as u32, "parent after child");
        assert!(d.name[i].is_some() && CoreKind::from_u8(d.kind[i]) != CoreKind::Module);
        assert!(d.start[i] <= d.end[i] && d.end[i] as usize <= src.len(), "def span out of bounds");
        assert!(d.start_line[i] <= d.name_line[i] && d.name_line[i] <= d.end_line[i], "def lines out of order");
        assert!(d.end_line[i] <= max_line, "def ends after the last line");
        let p = d.parent[i] as usize;
        assert!(d.start[p] <= d.start[i] && d.end[i] <= d.end[p], "child outside parent");
    }
    // Decorator and base rows: grouped by owner in def order, every row owned by a def
    // whose count includes it (no orphans).
    for (owner, lens, rows, what) in [
        (&st.decorators.owner, &d.decorators_len, ff.decorators, "decorator"),
        (&st.bases.owner, &d.bases_len, ff.bases, "base"),
    ] {
        let owner = &owner[rows.range()];
        assert!(owner.windows(2).all(|w| w[0] <= w[1]), "{what} rows not grouped by def");
        assert!(owner.iter().all(|&o| o >= 1 && o < n), "{what} owner out of range");
        let mut count = vec![0u32; n as usize];
        for &o in owner {
            count[o as usize] += 1;
        }
        for (i, &c) in count.iter().enumerate() {
            assert_eq!(c, u32::from(lens[ff.defs.start as usize + i]), "{what} count of def {i}");
        }
    }
    for (scopes, what) in [(&st.imports.scope, "import"), (&st.calls.scope, "call"), (&st.bindings.scope, "binding"), (&st.decls.scope, "decl")] {
        assert!(scopes.iter().all(|&s| s < n), "{what} scope out of range");
    }
    for (lines, what) in [(&st.imports.line, "import"), (&st.calls.line, "call"), (&st.bindings.line, "binding"), (&st.diags.line, "diag"), (&st.suppressions.line, "suppression")] {
        if let Some(l) = lines.iter().find(|&&l| l == 0 || l > max_line) {
            panic!("{what} line {l} out of range (file has {} lines)", ff.lines);
        }
    }
    assert!(v.evidence().windows(2).all(|w| w[0] < w[1]), "evidence not sorted and unique");
    for i in 0..metrics.len() {
        let r = metrics.row(i);
        // Class rows sum their body (possibly 0); functions start at 1.
        let function = CoreKind::from_u8(d.kind[i + 1]) == CoreKind::Function;
        assert!((r.cyclomatic >= 1 || !function) && r.loc >= 1, "metrics row {i}: {r:?}");
        assert!(r.mi.is_finite() && (0.0..=100.0).contains(&r.mi), "maintainability index out of range");
    }
    let by_spans = render(v, |s| spans.text(src, s));

    let mut interner = Interner::new();
    st.clear();
    let ff2 = cf::extract(src, &mut st, &mut interner);
    assert_eq!(ff, ff2, "extraction is not deterministic");
    let by_interner = render(st.file(&ff2), |s| interner.resolve(s));
    assert_eq!(by_spans, by_interner, "span and interner sinks disagree");
}

/// Token classes of the toy tokenizer.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Class {
    Space,
    Newline,
    Word,
    Str,
    Comment,
    Punct,
}

/// Next token starting at `pos < src.len()`: `(class, len)` with `len >= 1`.
fn next_token(src: &[u8], pos: usize) -> (Class, usize) {
    let rest = &src[pos..];
    let b = rest[0];
    let run = |pred: fn(u8) -> bool| rest.iter().take_while(|&&c| pred(c)).count();
    match b {
        b' ' | b'\t' | b'\x0c' => (Class::Space, run(|c| c == b' ' || c == b'\t' || c == b'\x0c')),
        b'\n' | b'\r' => (Class::Newline, 1),
        b'#' => (Class::Comment, run(|c| c != b'\n' && c != b'\r')),
        b'\'' | b'"' => {
            // Single-line string with backslash escapes; unterminated strings end at the newline.
            let mut i = 1;
            while i < rest.len() {
                match rest[i] {
                    b'\\' => i += 2,
                    b'\n' | b'\r' => break,
                    c if c == b => {
                        i += 1;
                        break;
                    }
                    _ => i += 1,
                }
            }
            (Class::Str, i.min(rest.len()))
        }
        c if c == b'_' || c.is_ascii_alphanumeric() || c >= 0x80 => {
            (Class::Word, run(|c| c == b'_' || c.is_ascii_alphanumeric() || c >= 0x80))
        }
        _ => (Class::Punct, 1),
    }
}

/// Demo target: tokenizes with a toy scanner and checks its invariants (tokens tile the input
/// exactly, bracket depth never underflows its saturating counter, line count matches).
fn demo(src: &[u8]) {
    let mut pos = 0usize;
    let mut tokens = 0u32;
    let mut lines = 0usize;
    let mut depth = 0u32;
    while pos < src.len() {
        let (class, len) = next_token(src, pos);
        assert!(len >= 1 && pos + len <= src.len(), "token out of bounds");
        match class {
            Class::Newline => lines += usize::from(src[pos] == b'\n'),
            Class::Punct => match src[pos] {
                b'(' | b'[' | b'{' => depth = depth.saturating_add(1),
                b')' | b']' | b'}' => depth = depth.saturating_sub(1),
                _ => {}
            },
            Class::Str => assert!(len >= 1 && (src[pos] == b'\'' || src[pos] == b'"')),
            Class::Space | Class::Word | Class::Comment => {}
        }
        tokens = tokens.wrapping_add(1);
        pos += len;
    }
    assert_eq!(pos, src.len(), "tokens must tile the input");
    // Strings never swallow a '\n' (a backslash may escape one), so every newline is seen once
    // unless escaped inside a string.
    assert!(lines <= src.iter().filter(|&&b| b == b'\n').count());
    black_box((tokens, depth));
}

fn selftest_panic(src: &[u8]) {
    if src.contains(&0) {
        panic!("selftest: NUL byte in input");
    }
    if src.windows(4).any(|w| w == b"{{{{") {
        panic!("selftest: four opening braces");
    }
    demo(src);
}

const BOM: &[u8] = "\u{FEFF}".as_bytes();

fn selftest_hang(src: &[u8]) {
    if src.windows(BOM.len()).any(|w| w == BOM) {
        // Bounded so abandoned threads in tests eventually finish.
        std::thread::sleep(Duration::from_millis(1500));
    }
    demo(src);
}

/// Recurses `depth` levels with a 256-byte frame each (opaque to the optimiser, so it cannot
/// become a loop).
fn recurse(depth: u64) -> u64 {
    let frame = black_box([depth as u8; 256]);
    if depth == 0 { u64::from(frame[0]) } else { recurse(black_box(depth - 1)).wrapping_add(u64::from(frame[255])) }
}

fn selftest_abort(src: &[u8]) {
    if src.contains(&b'`') {
        // No real stack is this deep: the guard page turns this into a fatal stack overflow,
        // which catch_unwind cannot catch.
        black_box(recurse(black_box(u64::MAX)));
    }
    demo(src);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_names_are_unique_and_default_is_not_a_selftest() {
        let mut names: Vec<&str> = TARGETS.iter().map(|t| t.name).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), TARGETS.len());
        assert!(!TARGETS[0].selftest);
        assert!(find("demo").is_some());
        assert!(find("nope").is_none());
    }

    #[test]
    fn demo_tokenizes_awkward_inputs() {
        for src in [
            &b""[..],
            b"'",
            b"'\\",
            b"\"abc\\\"",
            b"x = 'a\\\nb'\n",
            b"((((]]]]",
            b"#",
            b"\xff\xfe\0",
            b"def f(x):\n\treturn f'{x!r}'\n",
        ] {
            demo(src);
        }
        assert_eq!(next_token(b"'ab' x", 0), (Class::Str, 4));
        assert_eq!(next_token(b"'a\\'b'", 0), (Class::Str, 6));
        assert_eq!(next_token(b"'ab\nc'", 0), (Class::Str, 3));
        assert_eq!(next_token(b"abc_1 x", 0), (Class::Word, 5));
        assert_eq!(next_token(b"\t \x0cx", 0), (Class::Space, 3));
        assert_eq!(next_token(b"# c\nx", 0), (Class::Comment, 3));
    }

    /// Awkward inputs for the real targets (the fuzzer explores far more; these pin a few).
    const AWKWARD: &[&[u8]] = &[
        b"",
        b"\n",
        b"x",
        b"\xef\xbb\xbfdef f(): pass",
        b"def f(\n",
        b"class C(B, metaclass=M):\n    @p\n    def m(self): return f'{self.x!r:>{w}}'\n",
        b"if a:\n\tif b:\n  \t c()\n d()\n",
        b"x = (a\n  .b  # pymapper: ignore[PM1]\n  .c())\n",
        b"f'{'{'}' t'{x}' rb'\\' '''\n",
        b"\\\n\\",
        b"match x:\n    case [a, *b] if a: pass\n    case _: pass\n",
        b"print >>f, g(x)\nexec code\n",
        b"((((((((((((((((((((((((((((((((((((((((",
        b"\xff\xfe\x00 def \x80(): \x81()",
        b"@\ndef\nclass\nlambda: (yield)",
    ];

    #[test]
    fn real_targets_accept_awkward_inputs() {
        for &src in AWKWARD {
            lex(src);
            extract(src);
            parse(src);
            // Every prefix too: truncation is the fuzzer's most common mutation.
            for end in 0..src.len() {
                lex(&src[..end]);
                extract(&src[..end]);
            }
        }
    }

    #[test]
    fn lex_and_extract_on_corpus() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../tests/corpus");
        let w = pymapper_core::walk::walk(&[&dir], &pymapper_core::walk::WalkOptions::everything());
        assert!(w.files.len() >= 20);
        for i in 0..w.files.len() {
            let src = std::fs::read(w.files.get(i)).unwrap();
            lex(&src);
            extract(&src);
        }
    }

    #[test]
    fn selftests_trigger_on_their_patterns_only() {
        selftest_panic(b"x = 1\n");
        selftest_hang(b"x = 1\n");
        let nul = std::panic::catch_unwind(|| selftest_panic(b"a\0b"));
        assert!(nul.is_err());
        let braces = std::panic::catch_unwind(|| selftest_panic(b"{{{{"));
        assert!(braces.is_err());
    }
}
