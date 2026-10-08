//! Our side of `diff-ruff`: `pymapper_core::extract` (facts + metrics in one pass) normalised
//! into the comparable [`FileFacts`] model of [`super::facts`].
//!
//! Most fields come straight from the core fact tables. Two do not exist there as text,
//! because the core keeps only what the analyses need:
//!
//! - **decorators**: the core keeps each decorator's dotted chain plus `CALLED`/`COMPLEX`
//!   flags. The text is rebuilt from the chain (`a.b`), plus for a called decorator the
//!   normalised source from its `(` to the end of the decorator line; a `COMPLEX` decorator
//!   (no chain) uses the normalised source of the whole expression. The source is located by
//!   re-lexing from the def's start (the first `@`), one logical line per decorator. So a
//!   wrong chain, flag or decorator count shows up as a text mismatch.
//! - **bases**: the core keeps each base's chain, keyword and flags. The text between the
//!   class's parentheses is re-lexed and split at top-level commas; each item is rebuilt from
//!   its row (`kw=` + chain for a plain chain) or, for subscripted, starred or complex items,
//!   taken from the normalised source. A count mismatch between items and rows is marked
//!   with a trailing `!rows=N` so it cannot go unnoticed.
//!
//! Lines: defs use the name token's line (core `name_line`); calls use the line of the first
//! token of the callee chain (core call `line`), see the conventions in [`super::facts`].

use std::path::Path;

use pymapper_core::extract::facts::{
    self as cf, DefKind as CoreKind, FactStore, FileView, SpanStrs, base_flags, dec_flags, def_flags, import_flags,
};
use pymapper_core::extract::events::Receiver;
use pymapper_core::extract::{MetricsScratch, MetricsTable, MetricsVisitor};
use pymapper_core::intern::Sym;
use pymapper_core::lex::{Lexer, Token, TokenKind};

use super::facts::{Call, Def, DefKind, Extractor, FactSource, FileFacts, Import, LineIndex, MODULE_SCOPE, Skip, Str, normalise_expr};

/// `pymapper_core::extract` as a [`FactSource`].
pub struct Ours;

impl FactSource for Ours {
    fn name(&self) -> &'static str {
        "ours"
    }

    fn extractor(&self) -> Box<dyn Extractor + '_> {
        Box::new(OursExtractor::default())
    }
}

/// Per-thread state, reused across files.
#[derive(Default)]
pub struct OursExtractor {
    store: FactStore,
    spans: SpanStrs,
    scratch: MetricsScratch,
    metrics: MetricsTable,
    /// Scratch for one decorator line / class argument list.
    toks: Vec<Token>,
}

impl Extractor for OursExtractor {
    fn extract(&mut self, _path: &Path, src: &[u8], _lines: &LineIndex, out: &mut FileFacts) -> Result<(), Skip> {
        self.store.clear();
        self.metrics.clear();
        let m = MetricsVisitor::new(src, &mut self.scratch, &mut self.metrics);
        let (ff, _) = cf::extract_with(src, &mut self.store, &mut self.spans, m);
        let v = self.store.file(&ff);
        let cx = Cx { src, v, spans: &self.spans };
        cx.defs(&self.metrics, &mut self.toks, out);
        cx.imports(out);
        cx.calls(out);
        Ok(())
    }
}

/// Read-only view of one extracted file.
struct Cx<'a> {
    src: &'a [u8],
    v: FileView<'a>,
    spans: &'a SpanStrs,
}

/// Core scope (file-relative def row, 0 = `<module>`) -> model scope (def index without the
/// `<module>` row).
#[inline]
fn scope(s: u32) -> u32 {
    if s == 0 { MODULE_SCOPE } else { s - 1 }
}

impl<'a> Cx<'a> {
    #[inline]
    fn text(&self, s: Sym) -> &'a [u8] {
        self.spans.text(self.src, s)
    }

    /// Appends `chain` joined by `.`.
    fn push_chain(&self, chain: &[Sym], out: &mut Vec<u8>) {
        for (k, &s) in chain.iter().enumerate() {
            if k > 0 {
                out.push(b'.');
            }
            out.extend_from_slice(self.text(s));
        }
    }

    fn defs(&self, metrics: &MetricsTable, toks: &mut Vec<Token>, out: &mut FileFacts) {
        let st = &self.v.store.defs;
        for d in 1..self.v.def_count() {
            let row = self.v.def_row(d);
            let core = CoreKind::from_u8(st.kind[row]);
            let flags = st.flags[row];
            let kind = match core {
                CoreKind::Class => DefKind::Class,
                _ if flags & def_flags::ASYNC != 0 => DefKind::AsyncFunction,
                _ => DefKind::Function,
            };
            let parent = scope(st.parent[row]);
            let start = out.begin_str();
            if parent != MODULE_SCOPE {
                let q = out.defs[parent as usize].qualname;
                out.text.extend_from_within(q.off as usize..(q.off + q.len) as usize);
                out.text.push(b'.');
            }
            if let Some(n) = st.name[row] {
                out.text.extend_from_slice(self.text(n));
            }
            let qualname = out.end_str(start);
            let (decorators, bases) = self.header(d, core == CoreKind::Class, toks, out);
            let (params, mccabe) = match core {
                CoreKind::Class => (0, 0),
                // Metrics rows are in def_enter order without the `<module>` row.
                _ => (st.params_len[row] as u32, metrics.cyclomatic.get(d as usize - 1).map_or(0, |&c| u32::from(c))),
            };
            out.defs.push(Def { kind, parent, qualname, line: st.name_line[row], decorators, bases, params, mccabe });
        }
    }

    fn imports(&self, out: &mut FileFacts) {
        let im = &self.v.store.imports;
        for row in self.v.ff.imports.range() {
            let start = out.begin_str();
            self.push_chain(self.v.import_module(row), &mut out.text);
            let module = out.end_str(start);
            let name = if im.flags[row] & import_flags::STAR != 0 {
                out.push_str(b"*")
            } else {
                match im.name[row] {
                    Some(n) => out.push_str(self.text(n)),
                    None => Str::default(),
                }
            };
            let alias = match im.alias[row] {
                Some(a) => out.push_str(self.text(a)),
                None => Str::default(),
            };
            out.imports.push(Import {
                scope: scope(im.scope[row]),
                module,
                name,
                alias,
                level: im.level[row] as u32,
                line: im.line[row],
            });
        }
    }

    fn calls(&self, out: &mut FileFacts) {
        let c = &self.v.store.calls;
        for row in self.v.ff.calls.range() {
            let chain = self.v.call_chain(row);
            let start = out.begin_str();
            let prefix: &[u8] = match cf::receiver(c.receiver[row]) {
                Receiver::Super => b"super()",
                Receiver::Unknown => b"?",
                _ => b"",
            };
            out.text.extend_from_slice(prefix);
            if !prefix.is_empty() && !chain.is_empty() {
                out.text.push(b'.');
            }
            self.push_chain(chain, &mut out.text);
            let callee = out.end_str(start);
            out.calls.push(Call { scope: scope(c.scope[row]), callee, line: c.line[row] });
        }
    }

    /// Decorator and base texts of def `d` (see the module docs), from one lexer pass over the
    /// def's header starting at its first byte.
    fn header(&self, d: u32, class: bool, toks: &mut Vec<Token>, out: &mut FileFacts) -> (Str, Str) {
        let decs = self.v.decorators(d);
        if decs.is_empty() && !class {
            return (Str::default(), Str::default());
        }
        let start = self.v.store.defs.start[self.v.def_row(d)];
        let mut lx = Header { lx: Lexer::new(&self.src[start as usize..]), base: start };
        let dstart = out.begin_str();
        for (k, row) in decs.enumerate() {
            if k > 0 {
                out.text.push(b' ');
            }
            out.text.push(b'@');
            let at = lx.next_code();
            if at.kind != TokenKind::At {
                out.text.extend_from_slice(b"!missing");
                break;
            }
            toks.clear();
            loop {
                let t = lx.next();
                if matches!(t.kind, TokenKind::Newline | TokenKind::EndOfFile) {
                    break;
                }
                toks.push(t);
            }
            let end = toks.last().map_or(at.end(), |t| t.end());
            let chain = self.v.decorator_chain(row);
            let flags = self.v.store.decorators.flags[row];
            if chain.is_empty() || flags & dec_flags::COMPLEX != 0 {
                normalise_expr(&self.src[at.end() as usize..end as usize], &mut out.text);
            } else {
                self.push_chain(chain, &mut out.text);
                if flags & dec_flags::CALLED != 0 {
                    match toks.iter().find(|t| t.kind == TokenKind::LPar) {
                        Some(lp) => normalise_expr(&self.src[lp.start as usize..end as usize], &mut out.text),
                        None => out.text.extend_from_slice(b"!no-call"),
                    }
                }
            }
        }
        let decorators = out.end_str(dstart);
        if !class {
            return (decorators, Str::default());
        }
        let bstart = out.begin_str();
        // `class` keyword (a hard keyword: cannot occur in decorator expressions), name,
        // optional type parameters, then the argument list.
        loop {
            let t = lx.next();
            if matches!(t.kind, TokenKind::Class | TokenKind::EndOfFile) {
                break;
            }
        }
        lx.next(); // name
        let mut t = lx.next();
        if t.kind == TokenKind::LSqb {
            lx.skip_group(1);
            t = lx.next();
        }
        if t.kind == TokenKind::LPar {
            self.bases(d, &mut lx, toks, out);
        }
        (decorators, out.end_str(bstart))
    }

    /// Writes the bases text of class `d`; `lx` is just past the class's `(`.
    fn bases(&self, d: u32, lx: &mut Header<'_>, toks: &mut Vec<Token>, out: &mut FileFacts) {
        toks.clear();
        let mut depth = 0u32;
        loop {
            let t = lx.next();
            match t.kind {
                TokenKind::EndOfFile => break,
                TokenKind::LPar | TokenKind::LSqb | TokenKind::LBrace => depth += 1,
                TokenKind::RPar | TokenKind::RSqb | TokenKind::RBrace => {
                    if depth == 0 {
                        break;
                    }
                    depth -= 1;
                }
                _ => {}
            }
            toks.push(t);
        }
        let rows = self.v.bases(d);
        let nrows = rows.len();
        let mut rows = rows.into_iter();
        // Items: token ranges split at commas of depth 0 inside the parentheses.
        let mut items = 0usize;
        let mut depth = 0u32;
        let mut first = 0;
        for i in 0..=toks.len() {
            let split = match toks.get(i).map(|t| t.kind) {
                None => true,
                Some(TokenKind::Comma) => depth == 0,
                Some(TokenKind::LPar | TokenKind::LSqb | TokenKind::LBrace) => {
                    depth += 1;
                    false
                }
                Some(TokenKind::RPar | TokenKind::RSqb | TokenKind::RBrace) => {
                    depth = depth.saturating_sub(1);
                    false
                }
                _ => false,
            };
            if !split {
                continue;
            }
            if i > first {
                if items > 0 {
                    out.text.push(b',');
                }
                items += 1;
                let src = &self.src[toks[first].start as usize..toks[i - 1].end() as usize];
                self.base_item(rows.next(), src, out);
            }
            first = i + 1;
        }
        if items != nrows {
            out.text.extend_from_slice(b"!rows=");
            out.text.extend_from_slice(nrows.to_string().as_bytes());
        }
    }

    /// One base item: rebuilt from its row for a plain chain, else the normalised source.
    fn base_item(&self, row: Option<usize>, src: &[u8], out: &mut FileFacts) {
        if let Some(row) = row {
            let b = &self.v.store.bases;
            let chain = self.v.base_chain(row);
            if !chain.is_empty() && b.flags[row] & (base_flags::SUBSCRIPTED | base_flags::STARRED) == 0 {
                if let Some(kw) = b.keyword[row] {
                    out.text.extend_from_slice(self.text(kw));
                    out.text.push(b'=');
                }
                self.push_chain(chain, &mut out.text);
                return;
            }
        }
        normalise_expr(src, &mut out.text);
    }
}

/// Lexer over a suffix of the source starting at `base`, with offsets made absolute.
struct Header<'s> {
    lx: Lexer<'s>,
    base: u32,
}

impl Header<'_> {
    /// Next token with an absolute offset (layout tokens included).
    fn next(&mut self) -> Token {
        let mut t = self.lx.next_token();
        t.start += self.base;
        t
    }

    /// Next token that is not INDENT, DEDENT or NEWLINE.
    fn next_code(&mut self) -> Token {
        loop {
            let t = self.next();
            if !matches!(t.kind, TokenKind::Indent | TokenKind::Dedent | TokenKind::Newline) {
                return t;
            }
        }
    }

    /// Skips to just past the bracket closing a group whose opener was already consumed.
    fn skip_group(&mut self, mut depth: u32) {
        while depth > 0 {
            match self.next().kind {
                TokenKind::EndOfFile => return,
                TokenKind::LPar | TokenKind::LSqb | TokenKind::LBrace => depth += 1,
                TokenKind::RPar | TokenKind::RSqb | TokenKind::RBrace => depth -= 1,
                _ => {}
            }
        }
    }
}
