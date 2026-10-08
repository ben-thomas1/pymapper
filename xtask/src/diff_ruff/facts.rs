//! The normalised, comparable fact model shared by both sides of `diff-ruff`.
//!
//! Each side (the ruff oracle now, our extractor in S1.5) fills one [`FileFacts`] per file
//! by following the conventions below exactly; the diff engine then compares the two.
//!
//! Conventions (the contract a [`FactSource`] must follow):
//! - Lines are 1-based; `\n`, `\r\n` and a lone `\r` each end a line ([`LineIndex`]).
//! - A *scope* is the index of the innermost enclosing def (function or class) in
//!   [`FileFacts::defs`], or [`MODULE_SCOPE`] for the `<module>` pseudo-scope.
//!   Decorators, default values, annotations, type parameters and class bases belong to the
//!   scope *enclosing* the def (that is where Python evaluates them); lambdas and
//!   comprehensions do not open scopes.
//! - Defs are listed in source order (pre-order). `qualname` is the dotted path of enclosing
//!   def names plus the def's own name (`Outer.method.inner`; no `<locals>`). `line` is the line
//!   of the def's name token. `params` counts every parameter (positional-only, regular,
//!   `*args`, keyword-only, `**kwargs`, including `self`/`cls`). `mccabe` is ruff C901's score
//!   (functions only). Classes have `params == 0` and `mccabe == 0`; functions have empty
//!   `bases`.
//! - `decorators` is, in source order and separated by single spaces, `@` + [`normalise_expr`]
//!   of the source text after each decorator's `@` up to the end of its expression (parentheses
//!   of a parenthesised expression included; a trailing comment is dropped by the
//!   normalisation). The separator keeps `@a @b` (two decorators) apart from `@a@b` (one
//!   decorator `a @ b`): outside string literals, normalised text has no space next to `@`.
//!   `bases` is [`normalise_expr`] of the source text between the class's `(` and its matching
//!   `)` (positional bases, `*bases`, `metaclass=M`, `**kw`), minus one trailing top-level
//!   comma; empty without parentheses or with `()`.
//! - Imports: one fact per imported name, in source order. `import a.b as c` gives
//!   `module = "a.b"`, `name = ""`, `alias = "c"`, `level = 0`. `from ..a import b as c` gives
//!   `module = "a"`, `name = "b"`, `alias = "c"`, `level = 2`; `from . import x` has an empty
//!   module; star imports have `name = "*"`. `line` is the line of the imported name.
//! - Calls: one fact per call expression (nested calls each count), anywhere in the file
//!   including f-string replacement fields. `callee` is the receiver chain: segments joined
//!   by `.`, whose root is an identifier, `super()` (any call of the bare name `super`), or `?`
//!   for an unknown receiver (call result, subscript, literal, parenthesised expression of
//!   another kind, ...). Examples: `a.b.c(...)` -> `a.b.c`, `super().m()` -> `super().m`,
//!   `x[0].m()` -> `?.m`, `f()()` -> `?`. `line` is the line of the opening `(`.

use std::fmt;
use std::io::{self, Write};
use std::path::Path;

/// Scope value meaning the `<module>` pseudo-scope.
pub const MODULE_SCOPE: u32 = u32::MAX;

/// Display name of [`MODULE_SCOPE`].
pub const MODULE_SCOPE_NAME: &[u8] = b"<module>";

/// A byte range in [`FileFacts::text`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Str {
    pub off: u32,
    pub len: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DefKind {
    Function,
    AsyncFunction,
    Class,
}

impl DefKind {
    pub fn as_str(self) -> &'static str {
        match self {
            DefKind::Function => "function",
            DefKind::AsyncFunction => "async-function",
            DefKind::Class => "class",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Def {
    pub kind: DefKind,
    /// Enclosing def index or [`MODULE_SCOPE`].
    pub parent: u32,
    pub qualname: Str,
    pub line: u32,
    pub decorators: Str,
    pub bases: Str,
    pub params: u32,
    pub mccabe: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Import {
    pub scope: u32,
    pub module: Str,
    pub name: Str,
    pub alias: Str,
    pub level: u32,
    pub line: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Call {
    pub scope: u32,
    pub callee: Str,
    pub line: u32,
}

/// All facts of one file. Strings live in one per-file byte buffer (`text`) addressed by
/// [`Str`]; the struct is reused across files via [`FileFacts::clear`] so steady-state
/// extraction does not allocate.
#[derive(Default)]
pub struct FileFacts {
    pub text: Vec<u8>,
    pub defs: Vec<Def>,
    pub imports: Vec<Import>,
    pub calls: Vec<Call>,
}

impl FileFacts {
    pub fn clear(&mut self) {
        self.text.clear();
        self.defs.clear();
        self.imports.clear();
        self.calls.clear();
    }

    #[inline]
    pub fn str(&self, s: Str) -> &[u8] {
        &self.text[s.off as usize..(s.off + s.len) as usize]
    }

    /// Current end of `text`; pair with [`FileFacts::end_str`] to build a [`Str`] in place.
    #[inline]
    pub fn begin_str(&self) -> u32 {
        self.text.len() as u32
    }

    #[inline]
    pub fn end_str(&self, start: u32) -> Str {
        Str { off: start, len: self.text.len() as u32 - start }
    }

    pub fn push_str(&mut self, bytes: &[u8]) -> Str {
        let start = self.begin_str();
        self.text.extend_from_slice(bytes);
        self.end_str(start)
    }

    /// Qualified name of a scope (`<module>` for [`MODULE_SCOPE`]).
    pub fn scope_name(&self, scope: u32) -> &[u8] {
        if scope == MODULE_SCOPE {
            MODULE_SCOPE_NAME
        } else {
            self.str(self.defs[scope as usize].qualname)
        }
    }

    /// Writes every fact, one per line, in stored order (for `diff-ruff --dump`).
    pub fn dump(&self, out: &mut dyn Write) -> io::Result<()> {
        for d in &self.defs {
            writeln!(
                out,
                "def {} {} line={} params={} mccabe={} decorators={} bases={}",
                d.kind.as_str(),
                Bytes(self.str(d.qualname)),
                d.line,
                d.params,
                d.mccabe,
                Bytes(self.str(d.decorators)),
                Bytes(self.str(d.bases))
            )?;
        }
        for i in &self.imports {
            writeln!(
                out,
                "import {} module={} name={} alias={} level={} line={}",
                Bytes(self.scope_name(i.scope)),
                Bytes(self.str(i.module)),
                Bytes(self.str(i.name)),
                Bytes(self.str(i.alias)),
                i.level,
                i.line
            )?;
        }
        for c in &self.calls {
            writeln!(out, "call {} {} line={}", Bytes(self.scope_name(c.scope)), Bytes(self.str(c.callee)), c.line)?;
        }
        Ok(())
    }
}

/// Why a source could not produce facts for a file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SkipKind {
    NotUtf8,
    Syntax,
}

impl SkipKind {
    pub const COUNT: usize = 2;

    pub fn as_str(self) -> &'static str {
        match self {
            SkipKind::NotUtf8 => "non-utf8",
            SkipKind::Syntax => "syntax",
        }
    }
}

#[derive(Debug)]
pub struct Skip {
    pub kind: SkipKind,
    pub line: u32,
    pub message: String,
}

/// One side of the diff: something that turns source files into [`FileFacts`]. Shared by all
/// worker threads; each worker asks it for its own [`Extractor`].
pub trait FactSource: Sync {
    /// Short label used in reports ("ruff", "ours").
    fn name(&self) -> &'static str;

    /// Creates the per-thread extraction state (reusable buffers, arenas, interners). Called
    /// once per worker, not per file.
    fn extractor(&self) -> Box<dyn Extractor + '_>;
}

/// Per-thread extraction state of a [`FactSource`], reused across files. `Send` because rayon
/// may move a worker between threads between jobs (it is never shared).
pub trait Extractor: Send {
    /// Extracts the facts of `src` into `out`, which the caller has cleared. `lines` is the
    /// line index of `src`, built once per file by the driver.
    fn extract(&mut self, path: &Path, src: &[u8], lines: &LineIndex, out: &mut FileFacts) -> Result<(), Skip>;
}

/// Offset -> 1-based line lookup. Reusable across files.
#[derive(Default)]
pub struct LineIndex {
    starts: Vec<u32>,
}

impl LineIndex {
    pub fn reset(&mut self, src: &[u8]) {
        self.starts.clear();
        self.starts.push(0);
        let mut i = 0;
        let n = src.len();
        while i < n {
            match src[i] {
                b'\n' => self.starts.push(i as u32 + 1),
                b'\r' => {
                    if i + 1 < n && src[i + 1] == b'\n' {
                        i += 1;
                    }
                    self.starts.push(i as u32 + 1);
                }
                _ => {}
            }
            i += 1;
        }
    }

    /// Line containing byte `off`.
    #[inline]
    pub fn line(&self, off: u32) -> u32 {
        self.starts.partition_point(|&s| s <= off) as u32
    }
}

#[inline]
fn is_word(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b >= 0x80
}

/// Appends a canonical form of the Python expression source `src` to `out`: whitespace,
/// comments and backslash continuations outside string literals are removed, except that a
/// single space is kept where it separates two word characters (`not x`, `a if b else c`).
/// String literals are copied verbatim, f/t-strings including their replacement fields (with
/// PEP 701 nesting of any quote kind). Both sides of the diff must use this so the texts are
/// comparable regardless of formatting.
pub fn normalise_expr(src: &[u8], out: &mut Vec<u8>) {
    let n = src.len();
    // A separating space is only emitted between two bytes of this call's own output.
    let base = out.len();
    let mut i = 0;
    let mut pending_space = false;
    while i < n {
        let b = src[i];
        match b {
            b' ' | b'\t' | b'\n' | b'\r' | b'\x0c' => {
                pending_space = true;
                i += 1;
            }
            b'\\' if i + 1 < n && (src[i + 1] == b'\n' || src[i + 1] == b'\r') => {
                pending_space = true;
                i += 2;
            }
            b'#' => {
                i = line_end(src, i);
                pending_space = true;
            }
            _ => {
                if pending_space && out.len() > base && is_word(out[out.len() - 1]) && is_word(b) {
                    out.push(b' ');
                }
                pending_space = false;
                if b == b'"' || b == b'\'' {
                    let end = string_end(src, i);
                    out.extend_from_slice(&src[i..end]);
                    i = end;
                } else {
                    out.push(b);
                    i += 1;
                }
            }
        }
    }
}

/// Index of the line terminator at or after `i` (or `src.len()`).
fn line_end(src: &[u8], mut i: usize) -> usize {
    while i < src.len() && src[i] != b'\n' && src[i] != b'\r' {
        i += 1;
    }
    i
}

/// `(format, raw)` flags of the string prefix (up to three of `rbfut`, any case) just before
/// the quote at `src[quote]`. Letters glued to a longer identifier are not a prefix (`if"x"`).
fn string_prefix(src: &[u8], quote: usize) -> (bool, bool) {
    let mut j = quote;
    while j > 0 && quote - j < 3 && matches!(src[j - 1] | 0x20, b'r' | b'b' | b'f' | b'u' | b't') {
        j -= 1;
    }
    if j > 0 && is_word(src[j - 1]) {
        return (false, false);
    }
    let mut format = false;
    let mut raw = false;
    for &c in &src[j..quote] {
        match c | 0x20 {
            b'f' | b't' => format = true,
            b'r' => raw = true,
            _ => {}
        }
    }
    (format, raw)
}

/// Index just past the string literal whose opening quote is `src[start]` (or `src.len()` if
/// unterminated). Handles triple quotes and backslash escapes, and in f/t-strings `{{`,
/// `\N{...}` and replacement fields, which may contain strings using the same quote (PEP 701).
fn string_end(src: &[u8], start: usize) -> usize {
    let (format, raw) = string_prefix(src, start);
    let q = src[start];
    let n = src.len();
    let triple = start + 2 < n && src[start + 1] == q && src[start + 2] == q;
    let mut i = if triple { start + 3 } else { start + 1 };
    while i < n {
        match src[i] {
            b'\\' => {
                if format && !raw && src.get(i + 1) == Some(&b'N') && src.get(i + 2) == Some(&b'{') {
                    // Named escape: its braces do not open a replacement field.
                    i += 3;
                    while i < n && src[i] != b'}' && src[i] != q {
                        i += 1;
                    }
                    if i < n && src[i] == b'}' {
                        i += 1;
                    }
                } else {
                    i += 2;
                }
            }
            b if b == q => {
                if !triple {
                    return i + 1;
                }
                if i + 2 < n && src[i + 1] == q && src[i + 2] == q {
                    return i + 3;
                }
                i += 1;
            }
            b'{' if format => {
                if src.get(i + 1) == Some(&b'{') {
                    i += 2;
                } else {
                    i = field_end(src, i + 1);
                }
            }
            _ => i += 1,
        }
    }
    n
}

/// Index just past the `}` closing the f-string replacement field whose body starts at `i`
/// (or `src.len()`). Brackets nest, strings and comments are skipped, and a top-level `:`
/// starts the format spec.
fn field_end(src: &[u8], mut i: usize) -> usize {
    let n = src.len();
    let mut depth = 0u32;
    while i < n {
        match src[i] {
            b'\'' | b'"' => i = string_end(src, i),
            b'#' => i = line_end(src, i),
            b'(' | b'[' | b'{' => {
                depth += 1;
                i += 1;
            }
            b')' | b']' | b'}' if depth > 0 => {
                depth -= 1;
                i += 1;
            }
            b'}' => return i + 1,
            b':' if depth == 0 => return spec_end(src, i + 1),
            _ => i += 1,
        }
    }
    n
}

/// Index just past the `}` closing a format spec starting at `i`. The spec is literal text
/// (quotes included) except for nested `{fields}`.
fn spec_end(src: &[u8], mut i: usize) -> usize {
    let n = src.len();
    while i < n {
        match src[i] {
            b'{' => i = field_end(src, i + 1),
            b'}' => return i + 1,
            _ => i += 1,
        }
    }
    n
}

/// Lossy UTF-8 `Display` for fact bytes, without allocating.
pub struct Bytes<'a>(pub &'a [u8]);

impl fmt::Display for Bytes<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for chunk in self.0.utf8_chunks() {
            f.write_str(chunk.valid())?;
            if !chunk.invalid().is_empty() {
                f.write_str("\u{fffd}")?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn norm(s: &str) -> String {
        let mut out = Vec::new();
        normalise_expr(s.as_bytes(), &mut out);
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn normalise_strips_layout() {
        assert_eq!(norm("foo( a ,\n  b )"), "foo(a,b)");
        assert_eq!(norm("app.route(\n    '/x',  # the path\n    methods=['GET'],\n)"), "app.route('/x',methods=['GET'],)");
        assert_eq!(norm("a \\\n . b"), "a.b");
        assert_eq!(norm("a if  b\telse c"), "a if b else c");
        assert_eq!(norm("not  x"), "not x");
        assert_eq!(norm("x . y"), "x.y");
        assert_eq!(norm("  lead"), "lead");
    }

    #[test]
    fn normalise_keeps_strings_verbatim() {
        assert_eq!(norm("f( 'a  # b' )"), "f('a  # b')");
        assert_eq!(norm("f(\"\"\"x\n  y\"\"\" , 1)"), "f(\"\"\"x\n  y\"\"\",1)");
        assert_eq!(norm(r#"f('it\'s  x' , "q")"#), r#"f('it\'s  x',"q")"#);
        assert_eq!(norm("f(rb'a  b')"), "f(rb'a  b')");
        // Unterminated string: copied to the end without panicking.
        assert_eq!(norm("f('abc  "), "f('abc  ");
        assert_eq!(norm("f('''ab"), "f('''ab");
        assert_eq!(norm("f('\\"), "f('\\");
    }

    #[test]
    fn normalise_fstrings_copy_fields_verbatim() {
        // PEP 701: same-quote nesting, a `#` inside a nested string, spaces inside fields.
        assert_eq!(norm(r##"f( f"{x["#  k"]}" , 1)"##), r##"f(f"{x["#  k"]}",1)"##);
        assert_eq!(norm(r#"f(f"{ a  + b }"  )"#), r#"f(f"{ a  + b }")"#);
        assert_eq!(norm(r#"f(rf'{d['k']:>{w}}'  ,  2)"#), r#"f(rf'{d['k']:>{w}}',2)"#);
        assert_eq!(norm(r#"f(F"{{  x }}" ,  t"{y!r}")"#), r#"f(F"{{  x }}",t"{y!r}")"#);
        // Quotes in a format spec are literal text.
        assert_eq!(norm(r#"f(f"{x:'^9}" ,  1)"#), r#"f(f"{x:'^9}",1)"#);
        // `\N{...}` is an escape in a non-raw f-string, not a field.
        assert_eq!(norm(r#"f(f"\N{BULLET} {x}" ,  1)"#), r#"f(f"\N{BULLET} {x}",1)"#);
        // Multi-line field with a comment (3.12+), nested triple-quoted string.
        assert_eq!(norm("g(f'''{\n  a  # c }\n}'''  , 1)"), "g(f'''{\n  a  # c }\n}''',1)");
        // Not a prefix: `if` glued to a quote is a keyword followed by a plain string.
        assert_eq!(norm(r#"x if"{a  b"else  y"#), r#"x if"{a  b"else y"#);
        // Unterminated field: copied to the end without panicking.
        assert_eq!(norm("f'{a  ("), "f'{a  (");
    }

    #[test]
    fn normalise_never_panics_on_truncated_input() {
        let src = br#"f(rf'{d['k']:>{w}}', f"\N{BULLET} {x!r:{y}} {{z}}", '''a''' # c
            b"\\", t'{ {'q': 1}['q'] }') \"#;
        let mut out = Vec::new();
        for end in 0..=src.len() {
            for start in 0..end {
                out.clear();
                normalise_expr(&src[start..end], &mut out);
                assert!(out.len() <= end - start);
            }
        }
    }

    #[test]
    fn normalise_never_joins_with_earlier_output() {
        let mut out = b"Name".to_vec();
        normalise_expr(b"  Base ,  Mixin ", &mut out);
        assert_eq!(out, b"NameBase,Mixin");
    }

    #[test]
    fn normalise_crlf_and_comment_at_eof() {
        assert_eq!(norm("a(\r\n b) # c"), "a(b)");
        assert_eq!(norm("a(\r b)"), "a(b)");
    }

    #[test]
    fn line_index_handles_all_terminators() {
        let src = b"a\nb\r\nc\rd";
        let mut li = LineIndex::default();
        li.reset(src);
        assert_eq!(li.line(0), 1);
        assert_eq!(li.line(1), 1); // the '\n' itself
        assert_eq!(li.line(2), 2);
        assert_eq!(li.line(4), 2); // '\n' of "\r\n"
        assert_eq!(li.line(5), 3);
        assert_eq!(li.line(7), 4);
        li.reset(b"");
        assert_eq!(li.line(0), 1);
        li.reset(b"x\n");
        assert_eq!(li.line(2), 2);
    }

    #[test]
    fn file_facts_strings_and_scopes() {
        let mut f = FileFacts::default();
        let q = f.push_str(b"A");
        f.defs.push(Def {
            kind: DefKind::Class,
            parent: MODULE_SCOPE,
            qualname: q,
            line: 1,
            decorators: Str::default(),
            bases: Str::default(),
            params: 0,
            mccabe: 0,
        });
        assert_eq!(f.scope_name(0), b"A");
        assert_eq!(f.scope_name(MODULE_SCOPE), b"<module>");
        let s = f.begin_str();
        f.text.extend_from_slice(b"xy");
        assert_eq!(f.str(f.end_str(s)), b"xy");
        f.clear();
        assert!(f.text.is_empty() && f.defs.is_empty());
    }

    #[test]
    fn bytes_display_is_lossy() {
        assert_eq!(Bytes(b"a\xffb").to_string(), "a\u{fffd}b");
        assert_eq!(Bytes("é".as_bytes()).to_string(), "é");
    }
}
