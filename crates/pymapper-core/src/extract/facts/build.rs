//! The fact-building visitor.

use super::*;
use crate::extract::events::{
    Assign, Base, Block, BlockKind, Call, Decorator, Def, Diagnostic, End, IfCond, Import, Value, Visitor,
};
use crate::extract::events::DefKind as EvDefKind;
use crate::extract::Parser;
use crate::lex::{MAX_SOURCE_LEN, Suppression, Token};

// Block-context flags (inherited by nested blocks and defs).
const B_TC: u8 = 1 << 0;
const B_COND: u8 = 1 << 1;
const B_MAIN: u8 = 1 << 2;
const B_TRY_IE: u8 = 1 << 3;

#[derive(Clone, Copy, Debug)]
struct Frame {
    kind: BlockKind,
    /// Context flags inside this block.
    flags: u8,
    /// `If`/`Elif`: the condition shape.
    cond: IfCond,
    /// `Try`: first import row (store-absolute) of the try body.
    imp_start: u32,
}

/// Extractor scratch kept in the [`FactStore`] across files (never shrinks).
#[derive(Default, Debug)]
pub(super) struct Scratch {
    /// Def stack, file-relative rows; `[0]` is `<module>`.
    scopes: Vec<u32>,
    blocks: Vec<Frame>,
    evidence: EvidenceFilter,
    exports: Vec<Sym>,
}

/// Slots of the evidence dedup filter (power of two).
const EV_SLOTS: usize = 1024;

/// Direct-mapped "recently seen" filter in front of the evidence column: most
/// name occurrences repeat (`self`, locals), so this drops nearly all
/// duplicates with one compare; the few that slip through are removed by the
/// sort + dedup at the end of the file. Slots are tagged with a per-file
/// generation, so nothing is cleared between files.
#[derive(Debug)]
struct EvidenceFilter {
    slots: Box<[u64]>,
    generation: u32,
}

impl Default for EvidenceFilter {
    fn default() -> Self {
        EvidenceFilter { slots: vec![0; EV_SLOTS].into_boxed_slice(), generation: 0 }
    }
}

impl EvidenceFilter {
    fn begin_file(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        if self.generation == 0 {
            // Generation wrapped: stale tags could match again.
            self.slots.fill(0);
            self.generation = 1;
        }
    }

    /// Whether `h` must be recorded (not seen recently in this file).
    #[inline(always)]
    fn fresh(&mut self, h: u32) -> bool {
        let tag = (u64::from(self.generation) << 32) | u64::from(h);
        let slot = &mut self.slots[h as usize & (EV_SLOTS - 1)];
        if *slot == tag {
            return false;
        }
        *slot = tag;
        true
    }
}

/// One cached name chain: first token offset + length -> its run in `names`.
#[derive(Clone, Copy, Default)]
struct ChainKey {
    first: u32,
    len: u32,
    col: ChainCol,
}

struct Builder<'a, S: StrSink> {
    src: &'a [u8],
    st: &'a mut FactStore,
    strs: &'a mut S,
    sc: Scratch,
    /// Where this file's rows start.
    base: TableBases,
    file_flags: u8,
    /// Context flags of the innermost block (0 outside blocks).
    cur: u8,
    lambdas: u32,
    /// Last closed `if`/`elif` at block depth `.0` and its condition.
    last_if: Option<(usize, IfCond)>,
    /// Last closed `try` at block depth `.0`: its body's import rows.
    last_try: Option<(usize, u32, u32)>,
    /// Two most recent chains (dedup of repeated chains by token position).
    cache: [ChainKey; 2],
    /// Slot of `cache` to overwrite next.
    cache_next: usize,
    /// Def flags gathered from the decorators of the next def.
    dec_flags: u16,
}

pub(super) fn extract<S: StrSink>(src: &[u8], st: &mut FactStore, strs: &mut S) -> FileFacts {
    let truncated = src.len() > MAX_SOURCE_LEN;
    let src = if truncated { &src[..MAX_SOURCE_LEN] } else { src };
    strs.begin_file(src);
    let mut sc = std::mem::take(&mut st.scratch);
    sc.scopes.clear();
    sc.blocks.clear();
    sc.evidence.begin_file();
    sc.exports.clear();
    let base = st.bases();
    let module = st.defs.push(
        DefKind::Module as u8,
        0,
        None,
        NO_PARENT,
        0,
        len32(src.len()),
        1,
        1,
        0,
        0,
        0,
        0,
        0,
        0,
        0,
        0,
    );
    sc.scopes.push(module - base.defs);
    let b = Builder {
        src,
        st,
        strs,
        sc,
        base,
        file_flags: if truncated { file_flags::TRUNCATED } else { 0 },
        cur: 0,
        lambdas: 0,
        last_if: None,
        last_try: None,
        cache: [ChainKey::default(); 2],
        cache_next: 0,
        dec_flags: 0,
    };
    let mut p = Parser::new(src, b);
    p.parse_file();
    // The parser's line cursor sits near the end already: this is cheap.
    let end = p.pos(len32(src.len()));
    let lines = if end.col == 0 { end.line - 1 } else { end.line };
    let (mut b, sups) = p.finish();
    b.finish(&sups, lines)
}

impl<S: StrSink> Builder<'_, S> {
    #[inline]
    fn sym(&mut self, t: Token) -> Sym {
        self.strs.sym(self.src, t.start, t.len)
    }

    #[inline]
    fn text(&self, t: Token) -> &[u8] {
        t.text(self.src)
    }

    /// Records a name chain in `names` (reusing the run of an identical recent chain).
    /// Records a name chain ([`ChainCol`] encoding). Chains longer than
    /// [`MAX_CHAIN`] keep their first `MAX_CHAIN` names.
    fn chain(&mut self, toks: &[Token]) -> ChainCol {
        let toks = &toks[..toks.len().min(MAX_CHAIN)];
        let Some(first) = toks.first() else { return ChainCol::EMPTY };
        let len = toks.len() as u32;
        for k in &self.cache {
            if k.first == first.start && k.len == len {
                return k.col;
            }
        }
        let col = if len == 1 {
            ChainCol { c: self.sym(*first), len: 1 }
        } else {
            let start = len32(self.st.names.len()) - self.base.names;
            for &t in toks {
                let s = self.sym(t);
                self.st.names.push(s);
            }
            ChainCol { c: Sym::from_index(start), len: len as u8 }
        };
        self.cache[self.cache_next] = ChainKey { first: first.start, len, col };
        self.cache_next ^= 1;
        col
    }

    /// Records the evidence hash of `src[start..start + len]`.
    #[inline(always)]
    fn evidence(&mut self, start: u32, len: u32) {
        let h = evidence_hash(&self.src[start as usize..(start + len) as usize]);
        if self.sc.evidence.fresh(h) {
            self.st.evidence.push(h);
        }
    }

    /// Current scope (file-relative def row).
    #[inline]
    fn scope(&self) -> Scope {
        // Never empty: `<module>` is pushed first and never popped.
        *self.sc.scopes.last().unwrap_or(&0)
    }

    #[inline]
    fn def_kind(&self, d: Scope) -> DefKind {
        DefKind::from_u8(self.st.defs.kind[(self.base.defs + d) as usize])
    }

    /// Finalises the file: suppressions, line count, `__all__`, evidence, header.
    fn finish(&mut self, sups: &[Suppression], lines: u32) -> FileFacts {
        let src = self.src;
        let mut lc = LineCounter::default();
        for s in sups {
            let line = lc.line_at(src, s.start);
            let start = len32(self.st.names.len()) - self.base.names;
            for code in s.codes(src) {
                // `code` is a subslice of `src`: recover its offset.
                let off = code.as_ptr() as usize - src.as_ptr() as usize;
                let sym = self.strs.sym(src, off as u32, code.len() as u32);
                self.st.names.push(sym);
            }
            let len = len32(self.st.names.len()) - self.base.names - start;
            self.st.suppressions.push(line, Run { start, len }, s.all as u8);
        }
        let m = self.base.defs as usize;
        self.st.defs.end_line[m] = lines.max(1);
        self.st.exports.extend_from_slice(&self.sc.exports);
        sort_dedup_tail(&mut self.st.evidence, self.base.evidence as usize);
        let now = self.st.bases();
        let b = self.base;
        let run = |a: u32, z: u32| Run { start: a, len: z - a };
        let ff = FileFacts {
            defs: run(b.defs, now.defs),
            decorators: run(b.decorators, now.decorators),
            bases: run(b.bases, now.bases),
            params: run(b.params, now.params),
            imports: run(b.imports, now.imports),
            calls: run(b.calls, now.calls),
            bindings: run(b.bindings, now.bindings),
            decls: run(b.decls, now.decls),
            names: run(b.names, now.names),
            exports: run(b.exports, now.exports),
            evidence: run(b.evidence, now.evidence),
            diags: run(b.diags, now.diags),
            suppressions: run(b.suppressions, now.suppressions),
            lines,
            flags: self.file_flags,
        };
        self.st.scratch = std::mem::take(&mut self.sc);
        ff
    }
}

impl<S: StrSink> Visitor for Builder<'_, S> {
    fn decorator(&mut self, dec: &Decorator<'_>) {
        let chain = self.chain(dec.names);
        let mut flags = if dec.called { dec_flags::CALLED } else { 0 };
        match dec.names.last() {
            Some(&last) => self.dec_flags |= decorator_flags(self.text(last), dec.names.len()),
            None => {
                flags |= dec_flags::COMPLEX;
                self.dec_flags |= def_flags::OTHER_DECORATOR;
            }
        }
        self.st.decorators.push(chain.c, chain.len, flags);
    }

    fn base(&mut self, b: &Base<'_>) {
        let chain = self.chain(b.names);
        let keyword = b.keyword.map(|k| self.sym(k));
        let flags = if b.subscripted { base_flags::SUBSCRIPTED } else { 0 } | if b.starred { base_flags::STARRED } else { 0 };
        self.st.bases.push(chain.c, chain.len, keyword, flags);
    }

    fn def_enter(&mut self, def: &Def<'_>) {
        use def_flags::*;
        let parent = self.scope();
        let mut flags = std::mem::take(&mut self.dec_flags);
        let kind = match def.kind {
            EvDefKind::Function => DefKind::Function,
            EvDefKind::Class => DefKind::Class,
        };
        if def.is_async {
            flags |= ASYNC;
        }
        if def.has_return_annotation {
            flags |= RETURN_ANNOTATION;
        }
        let name = self.text(def.name);
        if name.len() > 4 && name.starts_with(b"__") && name.ends_with(b"__") {
            flags |= DUNDER;
        } else if name.first() == Some(&b'_') {
            flags |= PRIVATE;
        }
        match self.def_kind(parent) {
            DefKind::Class => flags |= METHOD,
            DefKind::Function => flags |= NESTED,
            DefKind::Module => {}
        }
        if self.cur & B_TC != 0 {
            flags |= TYPE_CHECKING;
        }
        if self.cur & B_COND != 0 {
            flags |= CONDITIONAL;
        }
        let decs = self.st.decorators.len() - self.base.decorators;
        // Counts beyond the column width keep the last rows (the decorators nearest the def).
        let ndec = def.decorators.min(decs).min(u8::MAX as u32);
        let nb = self.st.bases.len() - self.base.bases;
        let nbase = def.bases.min(nb).min(u8::MAX as u32);
        let pstart = self.st.params.len() - self.base.params;
        for p in def.params {
            let s = self.sym(p.name);
            let mut f = param_kind_u8(p.kind);
            if p.has_default {
                f |= param_flags::DEFAULT;
            }
            if p.annotated {
                f |= param_flags::ANNOTATED;
            }
            self.st.params.push(s, f);
        }
        let nparams = (self.st.params.len() - self.base.params - pstart).min(u16::MAX as u32);
        let name = self.sym(def.name);
        let row = self.st.defs.push(
            kind as u8,
            flags,
            Some(name),
            parent,
            def.start,
            def.start,
            def.start_line,
            def.start_line,
            def.name_pos.line,
            def.name_pos.col,
            decs - ndec,
            ndec as u8,
            nb - nbase,
            nbase as u8,
            pstart,
            nparams as u16,
        );
        self.sc.scopes.push(row - self.base.defs);
    }

    fn def_exit(&mut self, _kind: EvDefKind, end: End) {
        if self.sc.scopes.len() > 1 {
            let d = self.sc.scopes.pop().unwrap_or(0);
            let row = (self.base.defs + d) as usize;
            self.st.defs.end[row] = end.end;
            self.st.defs.end_line[row] = end.line;
        }
        self.dec_flags = 0;
    }

    fn block_enter(&mut self, b: &Block<'_>) {
        let depth = self.sc.blocks.len();
        let mut flags = self.cur;
        let mut imp_start = 0;
        match b.kind {
            BlockKind::If => {
                flags |= B_COND;
                match b.cond {
                    IfCond::TypeChecking => flags |= B_TC,
                    IfCond::MainGuard if self.scope() == 0 => {
                        flags |= B_MAIN;
                        self.file_flags |= file_flags::MAIN_GUARD;
                    }
                    _ => {}
                }
            }
            BlockKind::Elif => {
                flags |= B_COND;
                if b.cond == IfCond::TypeChecking {
                    flags |= B_TC;
                }
            }
            BlockKind::IfElse => {
                flags |= B_COND;
                if self.last_if == Some((depth, IfCond::NotTypeChecking)) {
                    flags |= B_TC;
                }
            }
            BlockKind::Try => {
                flags |= B_COND;
                imp_start = self.st.imports.len();
            }
            BlockKind::Except => {
                flags |= B_COND;
                let catches = b.except_names.iter().any(|&t| matches!(self.text(t), b"ImportError" | b"ModuleNotFoundError"));
                if catches {
                    flags |= B_TRY_IE;
                    if let Some((d, s, e)) = self.last_try
                        && d == depth
                    {
                        for f in &mut self.st.imports.flags[s as usize..e as usize] {
                            *f |= import_flags::TRY_IMPORT_ERROR;
                        }
                    }
                }
            }
            BlockKind::For
            | BlockKind::While
            | BlockKind::LoopElse
            | BlockKind::TryElse
            | BlockKind::Finally
            | BlockKind::Case => flags |= B_COND,
            BlockKind::With | BlockKind::Match => {}
        }
        self.sc.blocks.push(Frame { kind: b.kind, flags, cond: b.cond, imp_start });
        self.cur = flags;
    }

    fn block_exit(&mut self, _kind: BlockKind, _end: End, _last_case: bool) {
        if let Some(f) = self.sc.blocks.pop() {
            let depth = self.sc.blocks.len();
            match f.kind {
                BlockKind::If | BlockKind::Elif => self.last_if = Some((depth, f.cond)),
                BlockKind::Try => self.last_try = Some((depth, f.imp_start, self.st.imports.len())),
                _ => {}
            }
        }
        self.cur = self.sc.blocks.last().map_or(0, |f| f.flags);
    }

    fn lambda_enter(&mut self, _tok: Token) {
        self.lambdas += 1;
    }

    fn lambda_exit(&mut self, _end: u32) {
        self.lambdas = self.lambdas.saturating_sub(1);
    }

    fn call(&mut self, c: &Call<'_>) {
        // An over-long chain keeps its last names (the callee), with an unknown receiver.
        let (names, recv) = match c.names.len() > MAX_CHAIN {
            true => (&c.names[c.names.len() - MAX_CHAIN..], Receiver::Unknown),
            false => (c.names, c.receiver),
        };
        let chain = self.chain(names);
        let scope = self.scope();
        let mut flags = 0;
        if self.cur & B_MAIN != 0 {
            flags |= call_flags::MAIN_GUARD;
        }
        if self.cur & B_TC != 0 {
            flags |= call_flags::TYPE_CHECKING;
        }
        if self.lambdas > 0 {
            flags |= call_flags::LAMBDA;
        }
        if scope == 0 && c.names.len() == 2 && self.text(c.names[0]) == b"__all__" {
            self.file_flags |= file_flags::ALL_DYNAMIC;
        }
        self.st.calls.push(scope, chain.c, chain.len, receiver_u8(recv), flags, c.pos.line, c.pos.col);
    }

    fn import(&mut self, i: &Import<'_>) {
        let module = self.chain(i.module);
        let name = i.name.map(|t| self.sym(t));
        let alias = i.alias.map(|t| self.sym(t));
        let mut flags = 0;
        if i.from {
            flags |= import_flags::FROM;
        }
        if i.star {
            flags |= import_flags::STAR;
        }
        if self.cur & B_TC != 0 {
            flags |= import_flags::TYPE_CHECKING;
        }
        if self.cur & B_TRY_IE != 0 {
            flags |= import_flags::TRY_IMPORT_ERROR;
        }
        if self.cur & B_COND != 0 {
            flags |= import_flags::CONDITIONAL;
        }
        let level = i.level.min(255) as u8;
        let scope = self.scope();
        self.st.imports.push(scope, module.c, module.len, name, alias, level, flags, i.pos.line, i.pos.col);
    }

    fn assign(&mut self, a: &Assign<'_>) {
        // Value first: it usually repeats the chain of the call event just seen.
        // An unpacked target gets one element of the value: its shape is unknown.
        let (value, value_chain) = match a.value {
            Value::None => (ValueKind::None, ChainCol::EMPTY),
            _ if a.unpacked => (ValueKind::Other, ChainCol::EMPTY),
            Value::Call { names } => (ValueKind::Call, self.chain(names)),
            Value::Name(names) => (ValueKind::Name, self.chain(names)),
            Value::Strings(_) => (ValueKind::Strings, ChainCol::EMPTY),
            Value::Other => (ValueKind::Other, ChainCol::EMPTY),
        };
        let target = self.chain(a.target);
        let mut flags = 0;
        if a.unpacked {
            flags |= binding_flags::UNPACKED;
        }
        if a.starred {
            flags |= binding_flags::STARRED;
        }
        if self.cur & B_TC != 0 {
            flags |= binding_flags::TYPE_CHECKING;
        }
        if self.cur & B_COND != 0 {
            flags |= binding_flags::CONDITIONAL;
        }
        let scope = self.scope();
        if scope == 0 && a.target.len() == 1 && self.text(a.target[0]) == b"__all__" {
            self.dunder_all(a);
        }
        self.st.bindings.push(
            scope,
            target.c,
            target.len,
            assign_kind_u8(a.kind),
            flags,
            value as u8,
            value_chain.c,
            value_chain.len,
            a.pos.line,
        );
    }

    fn decl(&mut self, kind: crate::extract::events::DeclKind, names: &[Token]) {
        let scope = self.scope();
        for &t in names {
            let s = self.sym(t);
            self.st.decls.push(scope, decl_kind_u8(kind), s);
        }
    }

    #[inline]
    fn name(&mut self, tok: Token) {
        self.evidence(tok.start, tok.len);
    }

    #[inline]
    fn attribute(&mut self, tok: Token) {
        self.evidence(tok.start, tok.len);
    }

    fn string(&mut self, tok: Token) {
        let Some((s, l)) = string_content(self.src, tok) else { return };
        let src = self.src;
        let (ev, out) = (&mut self.sc.evidence, &mut self.st.evidence);
        ident_segments(&src[s as usize..(s + l) as usize], |seg| {
            let h = evidence_hash(seg);
            if ev.fresh(h) {
                out.push(h);
            }
        });
    }

    fn diagnostic(&mut self, d: &Diagnostic) {
        self.st.diags.push(diag_kind_u8(d.kind), d.pos.line, d.pos.col);
    }
}

impl<S: StrSink> Builder<'_, S> {
    /// Module-level `__all__` binding.
    fn dunder_all(&mut self, a: &Assign<'_>) {
        use crate::extract::events::AssignKind as K;
        self.file_flags |= file_flags::ALL_DEFINED;
        let append = match a.kind {
            K::Plain | K::Annotated => false,
            K::Augmented if self.text(a.op) == b"+=" => true,
            _ => {
                self.file_flags |= file_flags::ALL_DYNAMIC;
                return;
            }
        };
        match a.value {
            Value::Strings(strs) => {
                if !append {
                    self.sc.exports.clear();
                }
                for &t in strs {
                    match string_content(self.src, t) {
                        Some((s, l)) => {
                            let sym = self.strs.sym(self.src, s, l);
                            self.sc.exports.push(sym);
                        }
                        None => self.file_flags |= file_flags::ALL_DYNAMIC,
                    }
                }
            }
            // `__all__: list[str]` binds nothing.
            Value::None => {}
            _ => self.file_flags |= file_flags::ALL_DYNAMIC,
        }
    }
}

/// Sorts `v[from..]` and removes duplicates from it.
fn sort_dedup_tail(v: &mut Vec<u32>, from: usize) {
    v[from..].sort_unstable();
    let mut w = from;
    for r in from..v.len() {
        if w == from || v[r] != v[w - 1] {
            v[w] = v[r];
            w += 1;
        }
    }
    v.truncate(w);
}

/// `src[i]` ends a line: `\n`, or a `\r` not followed by `\n`.
#[inline(always)]
fn is_line_end(src: &[u8], i: usize) -> bool {
    match src[i] {
        b'\n' => true,
        b'\r' => src.get(i + 1) != Some(&b'\n'),
        _ => false,
    }
}

/// Forward line lookup for increasing offsets.
#[derive(Default)]
struct LineCounter {
    off: usize,
    line: u32,
}

impl LineCounter {
    fn line_at(&mut self, src: &[u8], off: u32) -> u32 {
        let end = (off as usize).min(src.len());
        if end < self.off {
            *self = LineCounter::default();
        }
        for i in self.off..end {
            self.line += is_line_end(src, i) as u32;
        }
        self.off = end;
        self.line + 1
    }
}

/// Content span `(start, len)` of a plain string literal token (prefix and quotes stripped).
pub(super) fn string_content(src: &[u8], t: Token) -> Option<(u32, u32)> {
    let s = t.text(src);
    let mut i = 0;
    while i < s.len() && s[i].is_ascii_alphabetic() {
        i += 1;
    }
    let q = *s.get(i)?;
    if q != b'"' && q != b'\'' {
        return None;
    }
    let ql = if s.len() >= i + 6 && s[i + 1] == q && s[i + 2] == q { 3 } else { 1 };
    let start = i + ql;
    let end = s.len().checked_sub(ql)?;
    if end < start || !s[end..].iter().all(|&b| b == q) {
        return None;
    }
    Some((t.start + start as u32, (end - start) as u32))
}

/// Identifier byte (non-ASCII bytes allowed, as in the lexer).
#[inline]
fn is_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b >= 0x80
}

/// Longest string literal considered as name evidence.
const MAX_EVIDENCE_STR: usize = 200;

/// Calls `f` with each segment when `s` looks like an identifier or dotted
/// path (`name`, `pkg.mod.func`); does nothing otherwise.
fn ident_segments(s: &[u8], mut f: impl FnMut(&[u8])) {
    if s.is_empty() || s.len() > MAX_EVIDENCE_STR {
        return;
    }
    for seg in s.split(|&b| b == b'.') {
        if seg.is_empty() || seg[0].is_ascii_digit() || !seg.iter().all(|&b| is_ident_byte(b)) {
            return;
        }
    }
    for seg in s.split(|&b| b == b'.') {
        f(seg);
    }
}

/// Def flags from one decorator's last name.
fn decorator_flags(last: &[u8], chain_len: usize) -> u16 {
    use def_flags::*;
    match last {
        b"property" | b"cached_property" => PROPERTY,
        b"setter" | b"getter" | b"deleter" if chain_len >= 2 => PROPERTY,
        b"staticmethod" => STATICMETHOD,
        b"classmethod" => CLASSMETHOD,
        b"overload" => OVERLOAD,
        b"abstractmethod" => ABSTRACT,
        b"abstractproperty" => ABSTRACT | PROPERTY,
        b"abstractclassmethod" => ABSTRACT | CLASSMETHOD,
        b"abstractstaticmethod" => ABSTRACT | STATICMETHOD,
        _ => OTHER_DECORATOR,
    }
}
