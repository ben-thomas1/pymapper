//! Expression token scanner.
//!
//! Expressions are not parsed into trees. A single forward pass over the
//! tokens keeps a bracket/context stack ([`Level`]) and a postfix-chain state,
//! which is enough to recognise calls and their callee chains, keyword
//! arguments, walrus targets, lambdas, comprehension `for`/`if` vs ternary
//! `if`, boolean-operator runs, and the overall shape of the expression
//! (plain name, single call, string list, ...). Statement keywords inside an
//! open bracket trigger unclosed-bracket recovery.

use super::events::{Assign, AssignKind, BoolOp, BranchKind, Call, DiagKind, Receiver, Value, Visitor};
use super::parser::Parser;
use crate::lex::{Token, TokenKind as K};

/// Stop conditions for [`Parser::expr`], checked at bracket depth 0 (relative
/// to where the scan started). NEWLINE, EOF, `;`, INDENT/DEDENT, an unmatched
/// closing bracket and statement-only keywords always stop.
pub(super) type Stop = u8;
/// `:` (compound header, annotation). Lambda colons are not stops.
pub(super) const S_COLON: Stop = 1;
/// `=` and augmented assignment operators.
pub(super) const S_ASSIGN: Stop = 2;
/// `,`.
pub(super) const S_COMMA: Stop = 4;
/// `as`.
pub(super) const S_AS: Stop = 8;
/// `in` (for-loop targets).
pub(super) const S_IN: Stop = 16;
/// `if` (case guard).
pub(super) const S_IF: Stop = 32;

/// Shape of a scanned expression (see [`Parser::expr`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Shape {
    /// No tokens.
    Empty,
    /// A plain dotted name; chain in `Parser::shape`.
    Name,
    /// A dotted name followed by one call; chain in `Parser::shape`.
    Call,
    /// A dotted name followed by one subscript; chain in `Parser::shape`.
    Subscript,
    /// A list/tuple/set display (or bare tuple) of plain strings; strings in `Parser::strings`.
    Strings,
    Other,
}

/// Statement-only hard keywords: never part of an expression.
#[inline]
pub(super) fn is_stmt_keyword(k: K) -> bool {
    matches!(
        k,
        K::Def
            | K::Class
            | K::Return
            | K::Import
            | K::While
            | K::Try
            | K::Except
            | K::Finally
            | K::With
            | K::Pass
            | K::Break
            | K::Continue
            | K::Raise
            | K::Global
            | K::Nonlocal
            | K::Del
            | K::Assert
            | K::Elif
    )
}

#[inline]
pub(super) fn is_augassign(k: K) -> bool {
    (k as u8) >= (K::PlusEqual as u8) && (k as u8) <= (K::DoubleSlashEqual as u8) && k != K::DoubleSlash
        || k == K::AtEqual
}

#[inline]
fn is_close(k: K) -> bool {
    matches!(k, K::RPar | K::RSqb | K::RBrace)
}

/// One open bracket (or the expression's top level) on the context stack.
#[derive(Clone, Copy, Default)]
pub(super) struct Level {
    /// Call argument list.
    call: bool,
    /// Argument list of a call of the bare name `super`.
    super_call: bool,
    /// Call, subscript or brace: names inside are not assignment targets.
    impure: bool,
    /// A comprehension `for` was seen at this level (a later `if` is a filter).
    comp: bool,
    /// Lambdas whose `:` has not been seen yet. u32 cannot overflow: each
    /// lambda takes at least 7 source bytes and sources are below 4 GiB.
    lambdas: u32,
    /// Lambda bodies open at this level.
    active: u32,
    /// Operator of the current boolean run (0 none, 1 and, 2 or).
    bool_last: u8,
}

/// Postfix-chain state: what the last token leaves for a following `.`, `(`, `[`.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Post {
    /// Expecting an operand.
    Operand,
    /// After a dotted name chain (in `Parser::chain`).
    Names,
    /// After a call result, subscript, literal or parenthesised group.
    Atom,
    /// After `.` following `Names` or `Atom`.
    Dot,
}

/// What a chain hangs off.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ChainBase {
    Plain,
    SelfName,
    ClsName,
    Super,
    Unknown,
}

/// Top-level shape recogniser state.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Sh {
    Empty,
    Name,
    NameDot,
    CallOpen,
    Call,
    SubOpen,
    Subscript,
    /// Inside a top-level display; `expect` = next must be a string (else a
    /// comma); `paren` = opened by `(` with no comma yet (`("a")` is a str).
    StrOpen { expect: bool, paren: bool },
    StrDone,
    /// Bare top-level tuple of strings.
    StrBare { expect: bool },
    Other,
}

impl Sh {
    fn finish(self, n_strings: usize) -> Shape {
        match self {
            Sh::Empty => Shape::Empty,
            Sh::Name => Shape::Name,
            Sh::Call => Shape::Call,
            Sh::Subscript => Shape::Subscript,
            Sh::StrDone => Shape::Strings,
            // A single bare string is not a tuple; `"a",` or `"a", "b"` is.
            Sh::StrBare { expect } if expect || n_strings > 1 => Shape::Strings,
            _ => Shape::Other,
        }
    }
}

/// Assignment-target segment state (between commas at a pure level).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Seg {
    Fresh,
    Star,
    Names { start: u32, starred: bool },
    NamesDot { start: u32, starred: bool },
    Bad,
}

/// A recorded simple assignment target: `Parser::tgt[start..start + len]`.
#[derive(Clone, Copy)]
pub(super) struct TargetRec {
    start: u32,
    len: u32,
    starred: bool,
    unpacked: bool,
}

/// Scanner-local state of one [`Parser::expr`] call.
struct Scan {
    post: Post,
    chain_base: ChainBase,
    prev: K,
    sh: Sh,
    seg: Seg,
    /// Open impure levels (calls, subscripts, braces).
    impure: u32,
    /// Open f/t-strings (a `!` conversion is only valid inside one).
    fstrings: u32,
}

impl<V: Visitor> Parser<'_, V> {
    /// Scans one expression (list) up to a stop token at depth 0, which is left
    /// unconsumed. Emits expression events and returns the expression's shape.
    /// With `self.collect` set, simple assignment targets are recorded into
    /// `self.tgt` / `self.tgt_recs`.
    pub(super) fn expr(&mut self, stop: Stop) -> Shape {
        let base = self.levels.len();
        self.levels.push(Level::default());
        self.shape.clear();
        self.strings.clear();
        let mut s = Scan {
            post: Post::Operand,
            chain_base: ChainBase::Plain,
            prev: K::Newline,
            sh: Sh::Empty,
            seg: Seg::Fresh,
            impure: 0,
            fstrings: 0,
        };
        loop {
            let t = self.peek(0);
            let depth = self.levels.len() - base - 1;
            let k = t.kind;
            match k {
                K::Newline | K::EndOfFile | K::Indent | K::Dedent => break,
                // Never part of an expression: ends it, recovering from an
                // unclosed bracket if one is open (also one opened before this scan).
                _ if is_stmt_keyword(k) => {
                    let recovered = self.recover_unclosed(t);
                    if depth == 0 || recovered {
                        break;
                    }
                }
                K::Colon if depth == 0 && stop & S_COLON != 0 && self.top().lambdas == 0 => break,
                K::Colon => {}
                _ if is_close(k) && depth == 0 => break,
                // `,` and `=` inside pending lambda parameters do not stop.
                K::Comma | K::Equal if self.top().lambdas > 0 => {}
                _ if depth == 0 && stops(k, stop) => break,
                _ => {}
            }
            if s.sh != Sh::Other {
                self.shape_step(&mut s.sh, depth, t);
            }
            if self.collect {
                self.target_step(&mut s, t);
            }
            if self.pattern {
                self.tgt.push(t);
            }
            if k == K::Name && s.post != Post::Dot && self.keyword_arg_at(&mut s, t) {
                continue;
            }
            self.scan_token(&mut s, t);
            s.prev = k;
            self.bump();
        }
        // Close everything still open (normal end: only the top level).
        while self.levels.len() > base {
            if let Some(l) = self.levels.pop() {
                // Malformed input can leave lambdas without their `:`.
                self.exit_lambdas(l.active + l.lambdas);
            }
        }
        if self.collect {
            self.finish_seg(&mut s.seg);
        }
        s.sh.finish(self.strings.len())
    }

    #[inline]
    fn top(&mut self) -> &mut Level {
        let n = self.levels.len() - 1;
        &mut self.levels[n]
    }

    #[inline]
    fn exit_lambdas(&mut self, n: u32) {
        for _ in 0..n {
            self.v.lambda_exit(self.last_end);
        }
    }

    /// `name=` in a call: reports it and consumes both tokens.
    #[inline]
    fn keyword_arg_at(&mut self, s: &mut Scan, t: Token) -> bool {
        let l = self.top();
        if !(l.call && l.lambdas == 0 && matches!(s.prev, K::LPar | K::Comma)) || self.kind(1) != K::Equal {
            return false;
        }
        if !self.pattern {
            self.v.keyword_arg(t);
        }
        if self.pattern {
            // `t` itself was recorded by the caller.
            let eq = self.peek(1);
            self.tgt.push(eq);
        }
        self.bump();
        self.bump();
        s.prev = K::Equal;
        true
    }
}

#[inline]
fn stops(k: K, stop: Stop) -> bool {
    match k {
        K::Semi => true,
        K::Equal => stop & S_ASSIGN != 0,
        K::Comma => stop & S_COMMA != 0,
        K::As => stop & S_AS != 0,
        K::In => stop & S_IN != 0,
        K::If => stop & S_IF != 0,
        _ => is_augassign(k) && stop & S_ASSIGN != 0,
    }
}

impl<V: Visitor> Parser<'_, V> {
    /// Updates the bracket stack, postfix chain and boolean runs for `t` and
    /// emits the corresponding events.
    #[inline]
    fn scan_token(&mut self, s: &mut Scan, t: Token) {
        let post = s.post;
        s.post = Post::Operand;
        match t.kind {
            K::Name => {
                if post == Post::Dot {
                    self.chain.push(t);
                    self.v.attribute(t);
                } else {
                    let l = self.top();
                    let param = l.lambdas > 0 && matches!(s.prev, K::Lambda | K::Comma | K::Star | K::DoubleStar);
                    // `!r` conversion in an f-string replacement field is not a name.
                    if !param && s.prev != K::Exclamation {
                        self.v.name(t);
                    }
                    self.chain.clear();
                    self.chain.push(t);
                    s.chain_base = match t.text(self.src) {
                        b"self" => ChainBase::SelfName,
                        b"cls" => ChainBase::ClsName,
                        _ => ChainBase::Plain,
                    };
                }
                s.post = Post::Names;
            }
            K::Dot => {
                if post == Post::Atom {
                    self.chain.clear();
                }
                if matches!(post, Post::Names | Post::Atom) {
                    s.post = Post::Dot;
                }
            }
            K::LPar => {
                let call = matches!(post, Post::Names | Post::Atom);
                let mut super_call = false;
                if call {
                    if post == Post::Atom {
                        self.chain.clear();
                    }
                    super_call = s.chain_base == ChainBase::Plain
                        && self.chain.len() == 1
                        && self.chain[0].text(self.src) == b"super";
                    let receiver = match (s.chain_base, self.chain.len()) {
                        (ChainBase::Super, _) => Receiver::Super,
                        (ChainBase::Unknown, _) => Receiver::Unknown,
                        (_, 0 | 1) => Receiver::None,
                        (ChainBase::Plain, _) => Receiver::Dotted,
                        (ChainBase::SelfName, _) => Receiver::SelfAttr,
                        (ChainBase::ClsName, _) => Receiver::ClsAttr,
                    };
                    if !self.pattern {
                        let first = self.chain.first().copied().unwrap_or(t);
                        let pos = self.pos(first.start);
                        self.v.call(&Call { names: &self.chain, receiver, lpar: t, pos });
                    }
                }
                s.impure += call as u32;
                self.levels.push(Level { call, super_call, impure: call, ..Level::default() });
            }
            K::LSqb | K::LBrace => {
                let impure = t.kind == K::LBrace || matches!(post, Post::Names | Post::Atom);
                s.impure += impure as u32;
                self.levels.push(Level { impure, ..Level::default() });
            }
            K::RPar | K::RSqb | K::RBrace => {
                // depth > 0 here: the caller stops at an unmatched closer.
                if let Some(l) = self.levels.pop() {
                    self.exit_lambdas(l.active + l.lambdas);
                    s.impure -= l.impure as u32;
                    s.chain_base = if l.super_call { ChainBase::Super } else { ChainBase::Unknown };
                }
                s.post = Post::Atom;
            }
            K::String => {
                self.v.string(t);
                s.chain_base = ChainBase::Unknown;
                s.post = Post::Atom;
            }
            K::FStringStart | K::TStringStart => s.fstrings += 1,
            K::FStringEnd | K::TStringEnd => {
                s.fstrings = s.fstrings.saturating_sub(1);
                s.chain_base = ChainBase::Unknown;
                s.post = Post::Atom;
            }
            K::Int | K::Float | K::Imaginary | K::True | K::False | K::None | K::Ellipsis => {
                s.chain_base = ChainBase::Unknown;
                s.post = Post::Atom;
            }
            K::Exclamation if s.fstrings == 0 => self.diag_at(DiagKind::UnexpectedToken, t),
            // Literal text keeps the state of the enclosing f-string.
            K::FStringMiddle | K::TStringMiddle => s.post = post,
            K::Lambda => {
                self.top().lambdas += 1;
                self.v.lambda_enter(t);
            }
            K::Colon => {
                let l = self.top();
                if l.lambdas > 0 {
                    l.lambdas -= 1;
                    l.active += 1;
                } else {
                    l.bool_last = 0;
                }
            }
            K::Comma => {
                let l = self.top();
                let n = l.active;
                l.active = 0;
                l.bool_last = 0;
                self.exit_lambdas(n);
            }
            K::For => {
                let l = self.top();
                let n = l.active;
                l.active = 0;
                l.bool_last = 0;
                l.comp = true;
                self.exit_lambdas(n);
                self.v.branch(BranchKind::CompFor, t);
            }
            K::If => {
                let l = self.top();
                l.bool_last = 0;
                let kind = if l.comp { BranchKind::CompIf } else { BranchKind::Ternary };
                self.v.branch(kind, t);
            }
            K::Else => self.top().bool_last = 0,
            K::And | K::Or => {
                let (op, code) = if t.kind == K::And { (BoolOp::And, 1) } else { (BoolOp::Or, 2) };
                let l = self.top();
                if l.bool_last != code {
                    l.bool_last = code;
                    self.v.bool_op(op, t);
                }
            }
            K::ColonEqual if post == Post::Names && self.chain.len() == 1 => {
                let target = self.chain[0];
                let pos = self.pos(target.start);
                self.v.assign(&Assign {
                    kind: AssignKind::Walrus,
                    target: &self.chain,
                    unpacked: false,
                    starred: false,
                    op: t,
                    value: Value::Other,
                    pos,
                });
            }
            _ => {}
        }
    }
}

impl<V: Visitor> Parser<'_, V> {
    /// Top-level shape recogniser; `depth` is the depth before `t`.
    #[inline]
    fn shape_step(&mut self, sh: &mut Sh, depth: usize, t: Token) {
        let k = t.kind;
        *sh = match *sh {
            Sh::Empty => match k {
                K::Name => {
                    self.shape.push(t);
                    Sh::Name
                }
                K::String => {
                    self.strings.push(t);
                    Sh::StrBare { expect: false }
                }
                K::LPar | K::LSqb | K::LBrace => Sh::StrOpen { expect: true, paren: k == K::LPar },
                _ => Sh::Other,
            },
            Sh::Name => match k {
                K::Dot => Sh::NameDot,
                K::LPar => Sh::CallOpen,
                K::LSqb => Sh::SubOpen,
                _ => Sh::Other,
            },
            Sh::NameDot if k == K::Name => {
                self.shape.push(t);
                Sh::Name
            }
            Sh::CallOpen if depth == 1 && is_close(k) => Sh::Call,
            Sh::SubOpen if depth == 1 && is_close(k) => Sh::Subscript,
            st @ (Sh::CallOpen | Sh::SubOpen) => st,
            Sh::StrOpen { expect, paren } => match k {
                K::String if expect => {
                    self.strings.push(t);
                    Sh::StrOpen { expect: false, paren }
                }
                K::Comma if !expect => Sh::StrOpen { expect: true, paren: false },
                // A parenthesised single string is not a tuple; `()` is.
                _ if is_close(k) && paren && !self.strings.is_empty() => Sh::Other,
                _ if is_close(k) => Sh::StrDone,
                _ => Sh::Other,
            },
            Sh::StrBare { expect } => match k {
                K::String if expect => {
                    self.strings.push(t);
                    Sh::StrBare { expect: false }
                }
                K::Comma if !expect => Sh::StrBare { expect: true },
                _ => Sh::Other,
            },
            _ => Sh::Other,
        };
    }

    /// Assignment-target recogniser (only with `self.collect`).
    #[inline]
    fn target_step(&mut self, s: &mut Scan, t: Token) {
        if s.impure > 0 {
            // Inside a call/subscript/brace: only its closer matters (handled by scan_token).
            return;
        }
        let k = t.kind;
        s.seg = match (s.seg, k) {
            (Seg::Fresh, K::Star) => Seg::Star,
            (Seg::Fresh | Seg::Star, K::Name) => {
                let start = self.tgt.len() as u32;
                self.tgt.push(t);
                Seg::Names { start, starred: s.seg == Seg::Star }
            }
            (Seg::Names { start, starred }, K::Dot) => Seg::NamesDot { start, starred },
            (Seg::NamesDot { start, starred }, K::Name) => {
                self.tgt.push(t);
                Seg::Names { start, starred }
            }
            (seg, K::Comma) => {
                let mut seg = seg;
                self.finish_seg(&mut seg);
                self.tgt_unpack = true;
                Seg::Fresh
            }
            // A grouping bracket at a segment start opens a nested unpacking.
            (Seg::Fresh | Seg::Star, K::LPar | K::LSqb) => {
                self.tgt_unpack = true;
                Seg::Fresh
            }
            (seg, K::RPar | K::RSqb) => {
                let mut seg = seg;
                self.finish_seg(&mut seg);
                // `(a).b = ...` and similar: whatever follows a closer is not a target.
                Seg::Bad
            }
            _ => Seg::Bad,
        };
    }

    /// Commits the segment if it is a complete simple target.
    #[inline]
    fn finish_seg(&mut self, seg: &mut Seg) {
        if let Seg::Names { start, starred } = *seg {
            let len = self.tgt.len() as u32 - start;
            self.tgt_recs.push(TargetRec { start, len, starred, unpacked: false });
        }
        *seg = Seg::Bad;
    }
}

/// Assignment statements and targets.
impl<V: Visitor> Parser<'_, V> {
    /// Scans one target group (or the value, which is scanned the same way)
    /// recording simple targets; marks them unpacked when the group has
    /// several targets or brackets.
    fn scan_group(&mut self, stop: Stop) -> Shape {
        let start = self.tgt_recs.len();
        self.tgt_unpack = false;
        self.collect = true;
        let shape = self.expr(stop);
        self.collect = false;
        if self.tgt_unpack || self.tgt_recs.len() - start > 1 {
            for r in &mut self.tgt_recs[start..] {
                r.unpacked = true;
            }
        }
        shape
    }

    /// Emits `assign` for recorded targets `from..to` with the value of `shape`.
    fn emit_assigns(&mut self, from: usize, to: usize, kind: AssignKind, op: Token, shape: Shape) {
        let value = match shape {
            Shape::Call => Value::Call { names: &self.shape },
            Shape::Name => Value::Name(&self.shape),
            Shape::Strings => Value::Strings(&self.strings),
            Shape::Empty => Value::None,
            Shape::Subscript | Shape::Other => Value::Other,
        };
        for r in &self.tgt_recs[from..to] {
            let target = &self.tgt[r.start as usize..(r.start + r.len) as usize];
            let pos = self.lines.pos(self.src, target[0].start);
            self.v.assign(&Assign { kind, target, unpacked: r.unpacked, starred: r.starred, op, value, pos });
        }
    }

    /// Expression statement or (annotated / augmented / chained) assignment.
    pub(super) fn expr_stmt(&mut self) {
        let first = self.peek(0);
        if first.kind == K::Name && self.python2_statement(first) {
            return;
        }
        self.tgt.clear();
        self.tgt_recs.clear();
        self.scan_group(S_ASSIGN | S_COLON);
        let k = self.kind(0);
        if k == K::Colon {
            let n = if self.tgt_recs.len() == 1 && !self.tgt_recs[0].unpacked { 1 } else { 0 };
            let colon = self.bump();
            self.expr(S_ASSIGN);
            let (op, shape) = match self.eat(K::Equal) {
                Some(eq) => (eq, self.expr(0)),
                None => (colon, Shape::Empty),
            };
            self.emit_assigns(0, n, AssignKind::Annotated, op, shape);
        } else if is_augassign(k) {
            let n = if self.tgt_recs.len() == 1 && !self.tgt_recs[0].unpacked { 1 } else { 0 };
            let op = self.bump();
            let shape = self.expr(0);
            self.emit_assigns(0, n, AssignKind::Augmented, op, shape);
        } else if k == K::Equal {
            let (op, value_start, shape) = loop {
                let op = self.bump();
                let value_start = self.tgt_recs.len();
                let shape = self.scan_group(S_ASSIGN);
                if !self.at(K::Equal) {
                    break (op, value_start, shape);
                }
            };
            self.emit_assigns(0, value_start, AssignKind::Plain, op, shape);
        }
        if self.last_end <= first.start {
            // Nothing consumed: a token that cannot start a statement here.
            let t = self.peek(0);
            if !matches!(t.kind, K::Newline | K::EndOfFile | K::Semi | K::Indent | K::Dedent) {
                let kind = if is_close(t.kind) { DiagKind::UnmatchedBracket } else { DiagKind::UnexpectedToken };
                self.diag_at(kind, t);
                self.bump();
            }
        }
    }

    /// `print x` / `print >>f, x` / `exec code`: reports Python 2 syntax and skips the statement.
    fn python2_statement(&mut self, first: Token) -> bool {
        let next = self.kind(1);
        let py2 = match first.text(self.src) {
            b"print" => matches!(next, K::String | K::Name | K::Int | K::Float | K::RightShift),
            b"exec" => matches!(next, K::String | K::Name),
            _ => false,
        };
        if py2 {
            self.diag_at(DiagKind::Python2, first);
            while !matches!(self.kind(0), K::Newline | K::EndOfFile | K::Semi | K::Indent | K::Dedent) {
                self.bump();
            }
        }
        py2
    }

    /// Loop (`for`) or `as` targets up to a stop token, reported as assignments without value.
    pub(super) fn targets(&mut self, op: Token, stop: Stop) {
        self.tgt.clear();
        self.tgt_recs.clear();
        self.scan_group(stop);
        let kind = if op.kind == K::For { AssignKind::ForTarget } else { AssignKind::AsTarget };
        let n = self.tgt_recs.len();
        self.emit_assigns(0, n, kind, op, Shape::Empty);
    }

    /// Type expression of an `except` clause; the last names of its plain dotted
    /// names (top level, unparenthesised list, or inside a tuple) go into `self.names`.
    pub(super) fn except_type(&mut self) {
        self.tgt.clear();
        self.tgt_recs.clear();
        self.scan_group(S_AS | S_COLON);
        self.names.clear();
        for r in &self.tgt_recs {
            self.names.push(self.tgt[(r.start + r.len - 1) as usize]);
        }
    }
}
