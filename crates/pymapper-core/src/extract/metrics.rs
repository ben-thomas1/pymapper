//! Per-definition metrics computed in the parse pass (design: Metric definitions).
//!
//! [`MetricsVisitor`] consumes parser events and appends one row per
//! definition to a [`MetricsTable`] (struct of arrays). Rows are in
//! `def_enter` order (pre-order: a definition before the ones nested in it),
//! which is the def index of the facts tables, so the table can be zipped with
//! (or folded into) the def records.
//!
//! Metrics per definition:
//! - `cyclomatic`: McCabe exactly as ruff C901 / PyCQA mccabe (statement-only).
//!   Functions: 1 + one per `if`, `elif`, `for`, `while`, `except` handler,
//!   `try`-`else`, `case` (minus 1 when the last case is irrefutable and
//!   unguarded), plus the full score of every nested function. Classes are
//!   transparent: their contents count towards the enclosing function. A
//!   class row holds the sum over its body (methods' scores plus class-level
//!   statements), a WMC-like number; it is not a C901 score.
//! - `cognitive`: SonarSource cognitive complexity. `if`, ternary, `match`,
//!   `for`, `while`, `except`: +1 + nesting. `elif` and every `else` (of `if`,
//!   loops and `try`): +1. Each run of like boolean operators: +1. Direct
//!   recursion: +1 once, for a bare call to the function's own name in a
//!   function whose enclosing scope is not a class, or `self.name(...)` /
//!   `cls.name(...)` in a method. (Class scope is not an enclosing scope, so a
//!   bare `name(...)` in a method is a global or builtin, e.g. a wrapper
//!   method `open` calling `open(...)`.) Nesting is increased by the bodies
//!   of `if`/`elif`/`else`, loops (and their `else`), `except`, `match`,
//!   lambdas and nested functions; `try`, `finally`, `with` and classes add
//!   nothing. In particular a `try`-`else` body costs +1 but, unlike an
//!   `if`/loop `else`, does not nest (it belongs to the `try`, which adds no
//!   nesting). Nested functions get their own score (nesting from 0) and
//!   count towards the parent at the parent's nesting + 1 (including any
//!   class-level blocks around a method of a class nested in a function). A
//!   class row holds the plain sum of its methods' scores and its
//!   class-level statements.
//!   Known approximation (tracked: needs a parser end event for ternaries,
//!   see `branch`): ternary operands are not nested
//!   (`a if (b if c else d) else e` scores 2, Sonar 3).
//! - `max_nesting`: maximum number of simultaneously open compound-statement
//!   bodies (`if`/`elif`/`else`, loops, `try`/`except`/`else`/`finally`,
//!   `with`, `case`; a `match` and its `case` count once) inside the
//!   definition, including nested definitions at the depth they sit at.
//! - `loc`: inclusive line span, from the definition's first line (first
//!   decorator) to its last token.
//! - Halstead counts from the tokens of the definition, from its `def` /
//!   `class` keyword to its end (decorators, and the `async` of an
//!   `async def`, belong to the enclosing scope).
//!   Operands: names (soft keywords included), numbers, strings, f/t-string
//!   literal text, `True`/`False`/`None`/`...`. Ignored: closing brackets
//!   (a bracket pair is one operator) and f/t-string ends. Operators:
//!   everything else (hard keywords, punctuation, f/t-string starts).
//!   Distinct operands are told apart by a 64-bit hash of their text.
//!   `volume = N log2 n` (0 when `n == 0`).
//! - `mi`: maintainability index
//!   `clamp((171 - 5.2 ln V - 0.23 CC - 16.2 ln LOC) * 100 / 171, 0, 100)`
//!   with `ln V` taken as 0 when `V < 1`.
//! - `params`: number of parameters (functions; 0 for classes).
//!
//! Cost: O(tokens) time plus one hash-map probe per operand token inside a
//! definition. Distinct counts are inclusive of nested definitions without
//! per-definition sets. Operators: a `u128` bitset of token kinds per open
//! definition, OR-ed into the parent when it closes. Operands: one "last seen
//! at token index" entry per distinct value; an occurrence is new exactly for
//! the innermost open definitions that started after its previous occurrence.

use super::events::{Block, BlockKind, BoolOp, BranchKind, Call, Def, DefKind, End, Receiver, Visitor};
use crate::intern::hash_bytes;
use crate::lex::{Token, TokenKind as K};

/// Metrics of all definitions of one or more files, struct of arrays, one row
/// per definition in `def_enter` order.
#[derive(Default, Debug, Clone, PartialEq)]
pub struct MetricsTable {
    pub cyclomatic: Vec<u32>,
    pub cognitive: Vec<u32>,
    /// Saturates at `u16::MAX`.
    pub max_nesting: Vec<u16>,
    pub loc: Vec<u32>,
    /// Halstead distinct operators (n1).
    pub distinct_operators: Vec<u32>,
    /// Halstead distinct operands (n2).
    pub distinct_operands: Vec<u32>,
    /// Halstead total operators (N1).
    pub operators: Vec<u32>,
    /// Halstead total operands (N2).
    pub operands: Vec<u32>,
    /// Halstead volume.
    pub volume: Vec<f32>,
    /// Maintainability index, 0..=100.
    pub mi: Vec<f32>,
    /// Saturates at `u16::MAX`.
    pub params: Vec<u16>,
}

/// One row of a [`MetricsTable`] (a copy; for tests and reporting).
#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub struct DefMetrics {
    pub cyclomatic: u32,
    pub cognitive: u32,
    pub max_nesting: u16,
    pub loc: u32,
    pub distinct_operators: u32,
    pub distinct_operands: u32,
    pub operators: u32,
    pub operands: u32,
    pub volume: f32,
    pub mi: f32,
    pub params: u16,
}

impl MetricsTable {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.cyclomatic.len()
    }

    pub fn is_empty(&self) -> bool {
        self.cyclomatic.is_empty()
    }

    pub fn clear(&mut self) {
        self.cyclomatic.clear();
        self.cognitive.clear();
        self.max_nesting.clear();
        self.loc.clear();
        self.distinct_operators.clear();
        self.distinct_operands.clear();
        self.operators.clear();
        self.operands.clear();
        self.volume.clear();
        self.mi.clear();
        self.params.clear();
    }

    /// Row `i` as a struct. Panics if out of range.
    pub fn row(&self, i: usize) -> DefMetrics {
        DefMetrics {
            cyclomatic: self.cyclomatic[i],
            cognitive: self.cognitive[i],
            max_nesting: self.max_nesting[i],
            loc: self.loc[i],
            distinct_operators: self.distinct_operators[i],
            distinct_operands: self.distinct_operands[i],
            operators: self.operators[i],
            operands: self.operands[i],
            volume: self.volume[i],
            mi: self.mi[i],
            params: self.params[i],
        }
    }

    /// Appends all rows of `other` (lock-free merge by concatenation).
    pub fn append(&mut self, other: &MetricsTable) {
        self.cyclomatic.extend_from_slice(&other.cyclomatic);
        self.cognitive.extend_from_slice(&other.cognitive);
        self.max_nesting.extend_from_slice(&other.max_nesting);
        self.loc.extend_from_slice(&other.loc);
        self.distinct_operators.extend_from_slice(&other.distinct_operators);
        self.distinct_operands.extend_from_slice(&other.distinct_operands);
        self.operators.extend_from_slice(&other.operators);
        self.operands.extend_from_slice(&other.operands);
        self.volume.extend_from_slice(&other.volume);
        self.mi.extend_from_slice(&other.mi);
        self.params.extend_from_slice(&other.params);
    }

    /// Heap bytes held (capacity).
    pub fn heap_bytes(&self) -> usize {
        2 * (self.max_nesting.capacity() + self.params.capacity())
            + 4 * (self.cyclomatic.capacity()
            + self.cognitive.capacity()
            + self.loc.capacity()
            + self.distinct_operators.capacity()
            + self.distinct_operands.capacity()
            + self.operators.capacity()
            + self.operands.capacity()
            + self.volume.capacity()
            + self.mi.capacity())
    }

    fn push(&mut self, m: &DefMetrics) {
        self.cyclomatic.push(m.cyclomatic);
        self.cognitive.push(m.cognitive);
        self.max_nesting.push(m.max_nesting);
        self.loc.push(m.loc);
        self.distinct_operators.push(m.distinct_operators);
        self.distinct_operands.push(m.distinct_operands);
        self.operators.push(m.operators);
        self.operands.push(m.operands);
        self.volume.push(m.volume);
        self.mi.push(m.mi);
        self.params.push(m.params);
    }

    fn set(&mut self, i: usize, m: &DefMetrics) {
        self.cyclomatic[i] = m.cyclomatic;
        self.cognitive[i] = m.cognitive;
        self.max_nesting[i] = m.max_nesting;
        self.loc[i] = m.loc;
        self.distinct_operators[i] = m.distinct_operators;
        self.distinct_operands[i] = m.distinct_operands;
        self.operators[i] = m.operators;
        self.operands[i] = m.operands;
        self.volume[i] = m.volume;
        self.mi[i] = m.mi;
        self.params[i] = m.params;
    }
}

/// Halstead volume `N log2 n`.
pub fn halstead_volume(n1: u32, n2: u32, big_n1: u32, big_n2: u32) -> f64 {
    let n = f64::from(n1) + f64::from(n2);
    if n <= 0.0 {
        return 0.0;
    }
    (f64::from(big_n1) + f64::from(big_n2)) * n.log2()
}

/// Maintainability index (see the module docs).
pub fn maintainability_index(volume: f64, cyclomatic: u32, loc: u32) -> f64 {
    let ln_v = if volume < 1.0 { 0.0 } else { volume.ln() };
    let ln_loc = f64::from(loc.max(1)).ln();
    let mi = (171.0 - 5.2 * ln_v - 0.23 * f64::from(cyclomatic) - 16.2 * ln_loc) * 100.0 / 171.0;
    mi.clamp(0.0, 100.0)
}

/// Halstead role of a token.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TokenRole {
    Operator,
    Operand,
    Ignored,
}

/// Halstead role of a token kind (see the module docs).
#[inline]
pub fn token_role(k: K) -> TokenRole {
    ROLES[k as usize]
}

// Operator kinds index a `u128` bitset.
const _: () = assert!((K::Exclamation as u8) < 128);

/// [`token_role`] by kind discriminant (one load instead of a branch tree).
static ROLES: [TokenRole; 256] = {
    let mut t = [TokenRole::Ignored; 256];
    let mut i = 0;
    while i <= K::Exclamation as usize {
        t[i] = TokenRole::Operator;
        i += 1;
    }
    let operands = [
        K::Name,
        K::Int,
        K::Float,
        K::Imaginary,
        K::String,
        K::FStringMiddle,
        K::TStringMiddle,
        K::False,
        K::None,
        K::True,
        K::Ellipsis,
    ];
    let mut j = 0;
    while j < operands.len() {
        t[operands[j] as usize] = TokenRole::Operand;
        j += 1;
    }
    let ignored = [
        K::EndOfFile,
        K::Newline,
        K::NonLogicalNewline,
        K::Indent,
        K::Dedent,
        K::Comment,
        K::RPar,
        K::RSqb,
        K::RBrace,
        K::FStringEnd,
        K::TStringEnd,
    ];
    let mut j = 0;
    while j < ignored.len() {
        t[ignored[j] as usize] = TokenRole::Ignored;
        j += 1;
    }
    t
};

/// Reference classification for [`ROLES`] (tests).
#[cfg(test)]
pub(super) fn token_role_slow(k: K) -> TokenRole {
    match k {
        K::Name
        | K::Int
        | K::Float
        | K::Imaginary
        | K::String
        | K::FStringMiddle
        | K::TStringMiddle
        | K::False
        | K::None
        | K::True
        | K::Ellipsis => TokenRole::Operand,
        K::EndOfFile
        | K::Newline
        | K::NonLogicalNewline
        | K::Indent
        | K::Dedent
        | K::Comment
        | K::RPar
        | K::RSqb
        | K::RBrace
        | K::FStringEnd
        | K::TStringEnd => TokenRole::Ignored,
        k if k.is_error() => TokenRole::Ignored,
        _ => TokenRole::Operator,
    }
}

/// Halstead state of an open definition.
#[derive(Clone, Copy, Debug)]
struct Hal {
    /// Token index of the `def`/`class` keyword (indices start at 1).
    tok_start: u32,
    /// Global operator / operand totals when the definition started.
    ops0: u32,
    opds0: u32,
    /// Operator kinds seen (distinct operators = popcount).
    ops_seen: u128,
    /// Distinct operands so far.
    n2: u32,
}

/// An open definition.
#[derive(Clone, Copy, Debug)]
struct Frame {
    row: u32,
    is_class: bool,
    name: Token,
    start_line: u32,
    params: u16,
    hal: Hal,
    /// McCabe: 1 (functions) or 0 (classes) plus everything inside so far.
    cc: u32,
    /// Cognitive score relative to this definition (nesting from 0).
    cog: u32,
    /// Nesting-weighted cognitive increments inside (nested definitions included).
    nk: u32,
    /// Classes: nesting increments of nested definitions not included in
    /// `cog` (per nesting-weighted increment inside a method: 1 for the
    /// method body plus the class-level blocks open around it).
    extra: u32,
    /// Current cognitive nesting.
    nest: u32,
    recursive: bool,
    /// Open compound-statement bodies / maximum seen.
    depth: u32,
    max_depth: u32,
}

/// Block flags on the block stack.
const B_NEST: u8 = 1;
const B_DEPTH: u8 = 2;
const B_IRREFUTABLE_CASE: u8 = 4;

/// Open-addressing map from an operand's 64-bit text hash to the token index
/// it was last seen at. Linear probing, load factor <= 1/2, slot index from
/// the hash's top bits (the best mixed). Key 0 marks an empty slot; keys have
/// their low bit forced to 1. Per file only the logical capacity (sized from
/// the source length) is cleared, so small files after a big one stay cheap.
#[derive(Default)]
struct LastSeen {
    slots: Vec<(u64, u32)>,
    /// Logical capacity: a power of two, `<= slots.len()`.
    cap: usize,
    shift: u32,
    len: usize,
    /// Scratch for [`LastSeen::grow`].
    tmp: Vec<(u64, u32)>,
}

impl LastSeen {
    const MIN_CAP: usize = 64;

    fn reset(&mut self, src_len: usize) {
        // About one distinct operand per 30-60 source bytes in real code.
        let cap = (src_len / 32).next_power_of_two().max(Self::MIN_CAP);
        self.set_cap(cap);
    }

    fn set_cap(&mut self, cap: usize) {
        if self.slots.len() < cap {
            self.slots.resize(cap, (0, 0));
        }
        self.slots[..cap].fill((0, 0));
        self.cap = cap;
        self.shift = 64 - cap.trailing_zeros();
        self.len = 0;
    }

    /// Sets `hash`'s entry to `val`; returns the previous value (0 if new).
    #[inline]
    fn insert(&mut self, hash: u64, val: u32) -> u32 {
        let key = hash | 1;
        let mask = self.cap - 1;
        let mut i = (key >> self.shift) as usize;
        loop {
            let s = &mut self.slots[i];
            if s.0 == key {
                return std::mem::replace(&mut s.1, val);
            }
            if s.0 == 0 {
                *s = (key, val);
                self.len += 1;
                if self.len * 2 > self.cap {
                    self.grow();
                }
                return 0;
            }
            i = (i + 1) & mask;
        }
    }

    /// Doubles the logical capacity and reinserts (rare: amortised O(1)).
    #[cold]
    fn grow(&mut self) {
        // Reused scratch: no allocation once it has reached its high-water mark.
        let mut old = std::mem::take(&mut self.tmp);
        old.clear();
        for &s in &self.slots[..self.cap] {
            if s.0 != 0 {
                old.push(s);
            }
        }
        self.set_cap(self.cap * 2);
        self.len = old.len();
        let mask = self.cap - 1;
        for &(key, val) in &old {
            let mut i = (key >> self.shift) as usize;
            while self.slots[i].0 != 0 {
                i = (i + 1) & mask;
            }
            self.slots[i] = (key, val);
        }
        self.tmp = old;
    }

    fn heap_bytes(&self) -> usize {
        (self.slots.capacity() + self.tmp.capacity()) * size_of::<(u64, u32)>()
    }
}

/// Reusable per-thread scratch of [`MetricsVisitor`]: keep one per worker and
/// pass it to every file; buffers keep their capacity.
#[derive(Default)]
pub struct MetricsScratch {
    frames: Vec<Frame>,
    blocks: Vec<u8>,
    /// Halstead state of a `def`/`class` keyword (its start offset) whose
    /// `def_enter` has not arrived yet: header tokens belong to the
    /// definition, header events to the enclosing scope.
    pending: Option<(u32, Hal)>,
    /// Last token index each operand (by text hash) was seen at.
    last_operand: LastSeen,
    tok: u32,
    ops: u32,
    opds: u32,
}

impl MetricsScratch {
    pub fn new() -> Self {
        Self::default()
    }

    fn reset(&mut self, src_len: usize) {
        self.frames.clear();
        self.blocks.clear();
        self.pending = None;
        self.last_operand.reset(src_len);
        self.tok = 0;
        self.ops = 0;
        self.opds = 0;
    }

    /// Heap bytes held (capacity).
    pub fn heap_bytes(&self) -> usize {
        self.frames.capacity() * size_of::<Frame>()
            + self.blocks.capacity()
            + self.last_operand.heap_bytes()
    }
}

/// Parser visitor computing [`MetricsTable`] rows for one file. Rows are
/// appended to `out` (row index = `out.len()` at `def_enter`).
pub struct MetricsVisitor<'a> {
    src: &'a [u8],
    s: &'a mut MetricsScratch,
    out: &'a mut MetricsTable,
}

impl<'a> MetricsVisitor<'a> {
    /// `src` must be the exact buffer given to the parser.
    pub fn new(src: &'a [u8], scratch: &'a mut MetricsScratch, out: &'a mut MetricsTable) -> Self {
        scratch.reset(src.len());
        MetricsVisitor { src, s: scratch, out }
    }
}

/// Parses `src` and returns the metrics of its definitions (convenience; for
/// bulk use keep a [`MetricsScratch`] and run [`MetricsVisitor`] alongside the
/// facts visitor).
pub fn compute(src: &[u8]) -> MetricsTable {
    let mut scratch = MetricsScratch::new();
    let mut out = MetricsTable::new();
    super::parse(src, MetricsVisitor::new(src, &mut scratch, &mut out));
    out
}

impl MetricsVisitor<'_> {
    /// Innermost open definition (a pending header excluded).
    #[inline]
    fn top(&mut self) -> Option<&mut Frame> {
        self.s.frames.last_mut()
    }

    /// Records a Halstead occurrence whose value was last seen at token index
    /// `prev`: it is new for every open definition that started after `prev`
    /// (a suffix of the stack, a pending header innermost).
    #[inline]
    fn distinct_operand(&mut self, prev: u32) {
        if let Some((_, h)) = &mut self.s.pending {
            if h.tok_start <= prev {
                return;
            }
            h.n2 += 1;
        }
        for f in self.s.frames.iter_mut().rev() {
            if f.hal.tok_start <= prev {
                return;
            }
            f.hal.n2 += 1;
        }
    }

    /// Drops a pending header that never reached `def_enter` (malformed
    /// input); its operators still count for the enclosing definition.
    #[inline]
    fn drop_pending(&mut self) {
        if let Some((_, h)) = self.s.pending.take()
            && let Some(f) = self.s.frames.last_mut()
        {
            f.hal.ops_seen |= h.ops_seen;
        }
    }

    /// Cognitive increment `+1 + nesting` on the innermost definition.
    #[inline]
    fn cog_nested(&mut self) {
        if let Some(f) = self.top() {
            // Saturating: pathological input (thousands of nested lambdas
            // around ternaries) is quadratic in the nesting.
            f.cog = f.cog.saturating_add(1 + f.nest);
            f.nk += 1;
        }
    }

    #[inline]
    fn cog_flat(&mut self) {
        if let Some(f) = self.top() {
            f.cog = f.cog.saturating_add(1);
        }
    }

    /// Closes the innermost frame and folds it into its parent.
    fn close(&mut self, end: End) {
        let Some(f) = self.s.frames.pop() else { return };
        let big_n1 = self.s.ops - f.hal.ops0;
        let big_n2 = self.s.opds - f.hal.opds0;
        let loc = end.line.saturating_sub(f.start_line) + 1;
        let cognitive = f.cog.saturating_add(u32::from(f.recursive));
        let n1 = f.hal.ops_seen.count_ones();
        let volume = halstead_volume(n1, f.hal.n2, big_n1, big_n2);
        let m = DefMetrics {
            cyclomatic: f.cc,
            cognitive,
            max_nesting: f.max_depth.min(u32::from(u16::MAX)) as u16,
            loc,
            distinct_operators: n1,
            distinct_operands: f.hal.n2,
            operators: big_n1,
            operands: big_n2,
            volume: volume as f32,
            mi: maintainability_index(volume, f.cc, loc) as f32,
            params: f.params,
        };
        self.out.set(f.row as usize, &m);
        if let Some(p) = self.s.frames.last_mut() {
            p.hal.ops_seen |= f.hal.ops_seen;
            // A nested function's full score (its base 1 is the +1 for the
            // nested def); a class is transparent.
            p.cc += f.cc;
            // Cognitive: increments inside `f` sit at the parent's nesting
            // (+1 for a function body). A class row is a plain sum of its
            // contents, so the method-body shifts it skips are kept in
            // `extra` and applied once the class reaches a function.
            if p.is_class {
                // Shift: the class-level blocks open around `f` (classes add
                // no nesting themselves) plus 1 for a method body.
                let shift = p.nest + u32::from(!f.is_class);
                p.cog = p.cog.saturating_add(cognitive);
                p.extra = p.extra.saturating_add(f.extra).saturating_add(f.nk.saturating_mul(shift));
            } else {
                let shift = p.nest + u32::from(!f.is_class);
                let shifted = f.nk.saturating_mul(shift).saturating_add(f.extra);
                p.cog = p.cog.saturating_add(cognitive).saturating_add(shifted);
            }
            p.nk += f.nk;
            p.max_depth = p.max_depth.max(p.depth + f.max_depth);
        }
    }
}

impl Visitor for MetricsVisitor<'_> {
    #[inline]
    fn token(&mut self, tok: Token) {
        let k = tok.kind;
        let role = token_role(k);
        if role == TokenRole::Ignored {
            return;
        }
        if matches!(k, K::Def | K::Class) {
            self.drop_pending();
            let s = &mut *self.s;
            s.pending = Some((tok.start, Hal { tok_start: s.tok + 1, ops0: s.ops, opds0: s.opds, ops_seen: 0, n2: 0 }));
        }
        let s = &mut *self.s;
        s.tok += 1;
        if role == TokenRole::Operator {
            s.ops += 1;
            let bit = 1u128 << (k as u32);
            if let Some((_, h)) = &mut s.pending {
                h.ops_seen |= bit;
            } else if let Some(f) = s.frames.last_mut() {
                f.hal.ops_seen |= bit;
            }
            return;
        }
        s.opds += 1;
        // Outside every definition no distinct counts change. Skipping the
        // last-seen update is sound: a stale entry is still below the start
        // index of every later definition.
        if s.frames.is_empty() && s.pending.is_none() {
            return;
        }
        let prev = s.last_operand.insert(hash_bytes(tok.text(self.src)), s.tok);
        self.distinct_operand(prev);
    }

    fn def_enter(&mut self, def: &Def<'_>) {
        let s = &mut *self.s;
        let hal = match s.pending.take() {
            Some((kw, h)) if kw == def.keyword.start => h,
            // Unreachable for parser input; header tokens are then not counted.
            _ => Hal { tok_start: s.tok + 1, ops0: s.ops, opds0: s.opds, ops_seen: 0, n2: 0 },
        };
        let is_class = def.kind == DefKind::Class;
        let row = self.out.len() as u32;
        self.out.push(&DefMetrics::default());
        s.frames.push(Frame {
            row,
            is_class,
            name: def.name,
            start_line: def.start_line,
            params: def.params.len().min(usize::from(u16::MAX)) as u16,
            hal,
            cc: u32::from(!is_class),
            cog: 0,
            nk: 0,
            extra: 0,
            nest: 0,
            recursive: false,
            depth: 0,
            max_depth: 0,
        });
    }

    fn def_exit(&mut self, _kind: DefKind, end: End) {
        self.drop_pending();
        self.close(end);
    }

    fn block_enter(&mut self, b: &Block<'_>) {
        // A header never contains a block: a pending one was malformed.
        self.drop_pending();
        use BlockKind as B;
        let (cc, flags) = match b.kind {
            B::If | B::For | B::While | B::Except => {
                self.cog_nested();
                (1, B_NEST | B_DEPTH)
            }
            B::Elif => {
                self.cog_flat();
                (1, B_NEST | B_DEPTH)
            }
            B::IfElse | B::LoopElse => {
                self.cog_flat();
                (0, B_NEST | B_DEPTH)
            }
            B::TryElse => {
                self.cog_flat();
                (1, B_DEPTH)
            }
            B::Try | B::Finally | B::With => (0, B_DEPTH),
            B::Match => {
                self.cog_nested();
                (0, B_NEST | B_DEPTH)
            }
            B::Case => (1, if b.irrefutable && !b.guarded { B_IRREFUTABLE_CASE } else { 0 }),
        };
        let applied = match self.s.frames.last_mut() {
            Some(f) => {
                f.cc += cc;
                if flags & B_NEST != 0 {
                    f.nest += 1;
                }
                if flags & B_DEPTH != 0 {
                    f.depth += 1;
                    f.max_depth = f.max_depth.max(f.depth);
                }
                flags
            }
            None => 0,
        };
        self.s.blocks.push(applied);
    }

    fn block_exit(&mut self, kind: BlockKind, _end: End, last_case: bool) {
        let flags = self.s.blocks.pop().unwrap_or(0);
        if flags == 0 {
            return;
        }
        if let Some(f) = self.s.frames.last_mut() {
            if flags & B_NEST != 0 {
                f.nest = f.nest.saturating_sub(1);
            }
            if flags & B_DEPTH != 0 {
                f.depth = f.depth.saturating_sub(1);
            }
            // An irrefutable unguarded last case is like an `else`.
            if kind == BlockKind::Case && last_case && flags & B_IRREFUTABLE_CASE != 0 {
                f.cc = f.cc.saturating_sub(1);
            }
        }
    }

    #[inline]
    fn branch(&mut self, kind: BranchKind, _tok: Token) {
        if kind == BranchKind::Ternary {
            // TODO(parser): with a ternary end event, nest its operands
            // (Sonar) instead of scoring them at the current nesting.
            self.cog_nested();
        }
    }

    #[inline]
    fn bool_op(&mut self, _op: BoolOp, _tok: Token) {
        self.cog_flat();
    }

    #[inline]
    fn lambda_enter(&mut self, _tok: Token) {
        if let Some(f) = self.top() {
            f.nest += 1;
        }
    }

    #[inline]
    fn lambda_exit(&mut self, _end: u32) {
        if let Some(f) = self.top() {
            f.nest = f.nest.saturating_sub(1);
        }
    }

    #[inline]
    fn call(&mut self, call: &Call<'_>) {
        let n = self.s.frames.len();
        let Some(f) = self.s.frames.last() else { return };
        if f.is_class || f.recursive {
            return;
        }
        let src = self.src;
        let name = f.name.text(src);
        let in_class = n >= 2 && self.s.frames[n - 2].is_class;
        let hit = match call.receiver {
            // Class scope is not an enclosing scope: a bare `name(...)` in a
            // method resolves to a global / builtin, never the method.
            Receiver::None => !in_class && call.names.len() == 1 && call.names[0].text(src) == name,
            Receiver::SelfAttr | Receiver::ClsAttr => {
                in_class && call.names.len() == 2 && call.names[1].text(src) == name
            }
            _ => false,
        };
        if hit {
            self.s.frames[n - 1].recursive = true;
        }
    }
}
