//! Source pane data: re-reads only a symbol's byte span from disk (sources are never retained
//! after extraction) and colours it with core's own lexer (no other highlighter).
//!
//! ```text
//!   (path, byte span) ──read_span──> bytes from the start of the span's first line
//!                     ──highlight──> text (lossy UTF-8) + style runs over it
//! ```
//!
//! Reading seeks to the span and reads at most [`MAX_SOURCE_BYTES`] (plus up to
//! [`LINE_BACKTRACK`] bytes before it, to start at the beginning of the first line). The lexer
//! runs over that fragment only; it never aborts, so a fragment cut mid-string or mid-bracket
//! still colours (the rest as an error/string token).

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;

use pymapper_core::lex::{Lexer, TokenKind};

/// Most bytes of a span read for the source pane (a module larger than this is cut).
pub const MAX_SOURCE_BYTES: u32 = 256 * 1024;
/// Bytes read before the span to find the start of its first line.
pub const LINE_BACKTRACK: u32 = 4096;

/// Colour class of a run of source text.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Style {
    Plain,
    Keyword,
    /// The name after `def` / `class`.
    DefName,
    /// `self`, `cls`, and common builtins.
    Builtin,
    /// `@decorator.chain`.
    Decorator,
    String,
    Number,
    Comment,
    Operator,
    /// Invalid input (lexer error tokens other than unterminated strings).
    Error,
}

/// A styled run: `text[start..end]`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Run {
    pub start: u32,
    pub end: u32,
    pub style: Style,
}

/// A highlighted fragment of a file.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Highlighted {
    /// The fragment (invalid UTF-8 replaced by U+FFFD).
    pub text: String,
    /// Style runs covering `text` in order, without gaps.
    pub runs: Vec<Run>,
    /// 1-based line number of the fragment's first line.
    pub first_line: u32,
    /// The span was longer than [`MAX_SOURCE_BYTES`] and was cut.
    pub truncated: bool,
}

impl Highlighted {
    /// Number of lines in the fragment.
    pub fn lines(&self) -> u32 {
        if self.text.is_empty() {
            return 0;
        }
        let nl = self.text.bytes().filter(|&c| c == b'\n').count() as u32;
        nl + u32::from(!self.text.ends_with('\n'))
    }

    /// Heap bytes.
    pub fn heap_bytes(&self) -> usize {
        self.text.capacity() + self.runs.capacity() * std::mem::size_of::<Run>()
    }
}

/// Bytes of `path` from the start of the line containing byte `start` up to `end` (at most
/// [`MAX_SOURCE_BYTES`] after `start`). Returns the bytes and whether they were cut.
pub fn read_span(path: &Path, start: u32, end: u32) -> io::Result<(Vec<u8>, bool)> {
    let mut f = File::open(path)?;
    let end = end.max(start);
    let back = start.min(LINE_BACKTRACK);
    let from = start - back;
    let want = end - start;
    let truncated = want > MAX_SOURCE_BYTES;
    let len = back + want.min(MAX_SOURCE_BYTES);
    f.seek(SeekFrom::Start(u64::from(from)))?;
    let mut buf = Vec::with_capacity(len as usize);
    f.take(u64::from(len)).read_to_end(&mut buf)?;
    // Start at the beginning of the first line: after the last newline before `start`. With
    // no newline in the backtrack window, the line started at the file start (`from == 0`) or
    // is longer than the window (start at the span itself).
    let back = (back as usize).min(buf.len());
    let cut = match buf[..back].iter().rposition(|&c| c == b'\n') {
        Some(i) => i + 1,
        None if from == 0 => 0,
        None => back,
    };
    buf.drain(..cut);
    Ok((buf, truncated))
}

/// `self`, `cls` and the builtins coloured as such (sorted for binary search).
const BUILTINS: [&[u8]; 40] = [
    b"Exception", b"NotImplemented", b"ValueError", b"all", b"any", b"bool", b"bytes", b"callable", b"classmethod", b"cls",
    b"dict", b"enumerate", b"filter", b"float", b"frozenset", b"getattr", b"hasattr", b"int", b"isinstance", b"issubclass",
    b"iter", b"len", b"list", b"map", b"max", b"min", b"next", b"object", b"open", b"print", b"property", b"range", b"repr",
    b"self", b"set", b"setattr", b"staticmethod", b"str", b"super", b"tuple",
];

fn is_builtin(name: &[u8]) -> bool {
    BUILTINS.binary_search(&name).is_ok()
}

fn token_style(kind: TokenKind) -> Style {
    use TokenKind as K;
    match kind {
        K::Comment => Style::Comment,
        K::Int | K::Float | K::Imaginary => Style::Number,
        K::String
        | K::FStringStart
        | K::FStringMiddle
        | K::FStringEnd
        | K::TStringStart
        | K::TStringMiddle
        | K::TStringEnd
        | K::ErrorUnterminatedString
        | K::ErrorUnterminatedTripleString => Style::String,
        k if k.is_keyword() => Style::Keyword,
        k if k.is_operator() => Style::Operator,
        k if k.is_error() => Style::Error,
        _ => Style::Plain,
    }
}

/// Highlights `src` (a fragment starting at line `first_line`).
pub fn highlight(src: &[u8], first_line: u32, truncated: bool) -> Highlighted {
    let mut out = Highlighted { text: String::with_capacity(src.len()), runs: Vec::with_capacity(src.len() / 4), first_line, truncated };
    let mut lx = Lexer::with_trivia(src);
    let mut pos = 0usize;
    // `def` / `class` seen: the next name is the defined name. `@` seen: names and dots
    // continue the decorator until another token.
    let mut after_def = false;
    let mut in_decorator = false;
    loop {
        let t = lx.next_token();
        if t.kind == TokenKind::EndOfFile {
            break;
        }
        if t.len == 0 {
            continue; // Indent, Dedent, zero-length errors
        }
        let (s, e) = (t.start as usize, t.end() as usize);
        if s > pos {
            push(&mut out, &src[pos..s], Style::Plain);
        }
        let text = &src[s..e];
        let style = match t.kind {
            TokenKind::Def | TokenKind::Class => {
                after_def = true;
                in_decorator = false;
                Style::Keyword
            }
            TokenKind::At if !in_decorator && lx_at_line_start(src, s) => {
                in_decorator = true;
                Style::Decorator
            }
            TokenKind::Name if after_def => {
                after_def = false;
                Style::DefName
            }
            TokenKind::Name | TokenKind::Dot if in_decorator => Style::Decorator,
            TokenKind::Name if is_builtin(text) => Style::Builtin,
            k => {
                in_decorator = false;
                if !k.is_trivia() {
                    after_def = false;
                }
                token_style(k)
            }
        };
        push(&mut out, text, style);
        pos = e;
    }
    if pos < src.len() {
        push(&mut out, &src[pos..], Style::Plain);
    }
    out
}

/// Whether only spaces/tabs precede byte `at` on its line (a decorator's `@`, not the matrix
/// multiplication operator).
fn lx_at_line_start(src: &[u8], at: usize) -> bool {
    src[..at].iter().rev().take_while(|&&c| c != b'\n').all(|&c| c == b' ' || c == b'\t' || c == b'\x0c')
}

/// Appends `bytes` with `style`, merging with the previous run when the style is equal.
fn push(out: &mut Highlighted, bytes: &[u8], style: Style) {
    let start = out.text.len() as u32;
    out.text.push_str(&String::from_utf8_lossy(bytes));
    let end = out.text.len() as u32;
    if end == start {
        return;
    }
    match out.runs.last_mut() {
        Some(r) if r.style == style && r.end == start => r.end = end,
        _ => out.runs.push(Run { start, end, style }),
    }
}

/// Reads and highlights the span `[start, end)` of `path` whose first line is `first_line`.
pub fn load(path: &Path, start: u32, end: u32, first_line: u32) -> io::Result<Highlighted> {
    let (bytes, truncated) = read_span(path, start, end)?;
    Ok(highlight(&bytes, first_line, truncated))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn styles(h: &Highlighted) -> Vec<(&str, Style)> {
        h.runs.iter().filter(|r| r.style != Style::Plain).map(|r| (&h.text[r.start as usize..r.end as usize], r.style)).collect()
    }

    #[test]
    fn builtins_sorted() {
        assert!(BUILTINS.windows(2).all(|w| w[0] < w[1]));
    }

    #[test]
    fn colours_python() {
        let src = b"@app.route(\"/\")\ndef handler(self, x=1):\n    # note\n    return len(f\"{x}\") @ m\n";
        let h = highlight(src, 10, false);
        assert_eq!(h.text.as_bytes(), src);
        let st = styles(&h);
        assert!(st.contains(&("@app.route", Style::Decorator)), "{st:?}");
        assert!(st.contains(&("def", Style::Keyword)));
        assert!(st.contains(&("handler", Style::DefName)));
        assert!(st.contains(&("self", Style::Builtin)));
        assert!(st.contains(&("1", Style::Number)));
        assert!(st.contains(&("# note", Style::Comment)));
        assert!(st.contains(&("return", Style::Keyword)));
        assert!(st.contains(&("len", Style::Builtin)));
        assert!(st.iter().any(|&(t, s)| s == Style::String && t.contains("f\"")), "{st:?}");
        // The matrix-multiply `@` mid-line is an operator, not a decorator.
        assert!(st.contains(&("@", Style::Operator)) || st.iter().any(|&(t, s)| s == Style::Operator && t.contains('@')), "{st:?}");
        assert_eq!(h.first_line, 10);
        assert_eq!(h.lines(), 4);
    }

    #[test]
    fn runs_cover_text_without_gaps() {
        let src = "class A:\n  x = 'é'  # ünï\n\tdef f(): pass\n".as_bytes();
        let h = highlight(src, 1, false);
        let mut pos = 0;
        for r in &h.runs {
            assert_eq!(r.start, pos);
            assert!(r.end > r.start);
            pos = r.end;
        }
        assert_eq!(pos as usize, h.text.len());
        assert!(h.runs.windows(2).all(|w| w[0].style != w[1].style), "adjacent equal runs merged");
    }

    #[test]
    fn fragments_and_invalid_input_never_fail() {
        // Cut inside a triple-quoted string, invalid UTF-8, stray bytes.
        for src in [&b"    x = \"\"\"never closed\n"[..], b"def f(\n  \xff\xfe = $\n", b"", b"\n\n", b"   ", b"@"] {
            let h = highlight(src, 1, false);
            assert!(h.text.len() >= src.len().saturating_sub(4));
            let total: u32 = h.runs.iter().map(|r| r.end - r.start).sum();
            assert_eq!(total as usize, h.text.len());
        }
        let h = highlight(b"x = \xff\n", 1, false);
        assert!(h.text.contains('\u{fffd}'));
    }

    #[test]
    fn read_span_starts_at_line_and_caps() {
        let dir = std::env::temp_dir().join(format!("pm_src_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("m.py");
        let text = b"import os\n\nclass A:\n    def m(self):\n        return 1\n\nx = 2\n";
        std::fs::write(&p, text).unwrap();
        // Span of `def m` starts at its keyword; the read starts at its line.
        let start = text.windows(3).position(|w| w == b"def").unwrap() as u32;
        let end = (text.windows(8).position(|w| w == b"return 1").unwrap() + 8) as u32;
        let (b, cut) = read_span(&p, start, end).unwrap();
        assert!(!cut);
        assert_eq!(b, b"    def m(self):\n        return 1");
        let h = load(&p, start, end, 4).unwrap();
        assert_eq!(h.first_line, 4);
        assert_eq!(h.lines(), 2);
        // A span at the file start.
        let (b, _) = read_span(&p, 0, 9).unwrap();
        assert_eq!(b, b"import os");
        // A span past the end reads what exists.
        let (b, _) = read_span(&p, 60, 10_000).unwrap();
        assert!(b.len() < 20);
        // A huge span is capped.
        let big = dir.join("big.py");
        let mut data = Vec::new();
        while data.len() < (MAX_SOURCE_BYTES as usize) * 2 {
            data.extend_from_slice(b"x = 1\n");
        }
        std::fs::write(&big, &data).unwrap();
        let (b, cut) = read_span(&big, 0, data.len() as u32).unwrap();
        assert!(cut);
        assert_eq!(b.len(), MAX_SOURCE_BYTES as usize);
        // A line longer than the backtrack window starts at the span.
        let long = dir.join("long.py");
        let mut data = vec![b'#'; (LINE_BACKTRACK as usize) * 2];
        data.extend_from_slice(b"\ndef f(): pass\n");
        std::fs::write(&long, &data).unwrap();
        let (b, _) = read_span(&long, LINE_BACKTRACK + 100, LINE_BACKTRACK + 110).unwrap();
        assert_eq!(b.len(), 10);
        assert!(read_span(&dir.join("missing.py"), 0, 1).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
