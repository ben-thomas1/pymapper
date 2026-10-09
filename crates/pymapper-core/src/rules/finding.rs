//! The finding model: compact records with typed message arguments, formatted only at output.

use std::cmp::Ordering;
use std::fmt;

use super::{Rule, RuleDef};

/// Index of a file in the global file table. [`FileId::NONE`] marks project-level findings that
/// have no file.
///
/// Defined here until the index (S2.3) owns the file table; it should then move to `index` and
/// be re-exported from this module so analysis code keeps compiling.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FileId(pub u32);

impl FileId {
    /// No file.
    pub const NONE: FileId = FileId(u32::MAX);
}

/// Index of a symbol (distribution, package, module, class, function) in the global symbol
/// table. Indices are dense and per run; the symbol's stable qualified id
/// (`pkg.mod:Class.method`), resolved through the index at output time, is what baselines key
/// on. [`SymbolId::NONE`] marks findings not attached to a symbol.
///
/// Defined here until the index (S2.3) exists; see [`FileId`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SymbolId(pub u32);

impl SymbolId {
    /// No symbol.
    pub const NONE: SymbolId = SymbolId(u32::MAX);
}

/// Source position of a finding.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Location {
    /// File (or [`FileId::NONE`]).
    pub file: FileId,
    /// 1-based line; 0 when the finding has no line (project-level).
    pub line: u32,
    /// 1-based column in bytes from the start of the line; 0 when unknown. Output converts to
    /// characters if it needs to (by re-reading the line; sources are not retained).
    pub col: u32,
}

impl Location {
    /// A position in a file.
    #[inline]
    pub fn new(file: FileId, line: u32, col: u32) -> Location {
        Location { file, line, col }
    }
}

/// Kind of a message argument; [`RuleDef::args`] declares one per placeholder.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ArgKind {
    /// Unsigned integer (metric values, limits, counts, line numbers).
    U32,
    /// Real number, printed with one decimal.
    F32,
    /// A symbol, printed as its qualified name.
    Symbol,
    /// An interned string (import names, distribution names).
    Name,
    /// A file, printed as its path.
    File,
    /// An import contract, by index into `Config::contracts`, printed as its name.
    Contract,
    /// One of a fixed set of wordings ([`Arg::Phrase`] indexes it), so a rule's message can
    /// vary while staying a template (`is never used` / `is only used by unused code`).
    Phrase(&'static [&'static str]),
}

/// A typed message argument (8 bytes). Strings are never stored: names are interned ids.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Arg {
    /// Unsigned integer.
    U32(u32),
    /// Real number.
    F32(f32),
    /// Symbol.
    Symbol(SymbolId),
    /// Interned string id (the raw value of an `intern::Sym`).
    Name(u32),
    /// File.
    File(FileId),
    /// Index into `Config::contracts`.
    Contract(u32),
    /// Index into the wordings of an [`ArgKind::Phrase`].
    Phrase(u32),
}

impl Arg {
    /// Whether the argument is of kind `k` (a phrase index must be in range).
    #[inline]
    pub fn fits(self, k: ArgKind) -> bool {
        match (self, k) {
            (Arg::U32(_), ArgKind::U32)
            | (Arg::F32(_), ArgKind::F32)
            | (Arg::Symbol(_), ArgKind::Symbol)
            | (Arg::Name(_), ArgKind::Name)
            | (Arg::File(_), ArgKind::File)
            | (Arg::Contract(_), ArgKind::Contract) => true,
            (Arg::Phrase(i), ArgKind::Phrase(words)) => (i as usize) < words.len(),
            _ => false,
        }
    }
}

/// Renders the id-typed arguments of a message. Implemented by the index for output and by the
/// GUI; implementations write straight into `out` (no intermediate `String`).
pub trait ArgResolver {
    /// Writes a symbol's qualified name.
    fn write_symbol(&self, id: SymbolId, out: &mut dyn fmt::Write) -> fmt::Result;
    /// Writes an interned string.
    fn write_name(&self, id: u32, out: &mut dyn fmt::Write) -> fmt::Result;
    /// Writes a file's display path.
    fn write_file(&self, id: FileId, out: &mut dyn fmt::Write) -> fmt::Result;
    /// Writes a contract's name.
    fn write_contract(&self, index: u32, out: &mut dyn fmt::Write) -> fmt::Result;
}

/// One finding (40 bytes). The message is the rule's template applied to the arguments stored
/// in the owning [`Findings`]; structured data that is not part of the message (a cycle's
/// members, the other copy of a duplicate) follows them as *extra* arguments.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Finding {
    /// The rule.
    pub rule: Rule,
    /// Confidence 0-100 (see the rule's [`ConfidenceModel`](super::ConfidenceModel)).
    pub confidence: u8,
    /// Where it is.
    pub loc: Location,
    /// Last line (inclusive) of the reported span; 0 when the analysis did not set one (output
    /// then derives it from the symbol's definition, else uses `loc.line`).
    pub end_line: u32,
    /// The symbol it is about (or [`SymbolId::NONE`]).
    pub symbol: SymbolId,
    /// Machine-readable reasons for the confidence: a bit set over the rule family's reason
    /// names (dead code: [`super::dead_code::reason`]; 0 = none). Rendered by output only.
    pub reasons: u32,
    args_start: u32,
    args_len: u32,
    /// Extra (data-only) arguments, stored right after the message arguments.
    extra_len: u32,
}

/// A buffer of findings and their arguments. Build one per thread and merge with
/// [`append`](Self::append); clear and reuse buffers instead of reallocating.
#[derive(Clone, Debug, Default)]
pub struct Findings {
    items: Vec<Finding>,
    args: Vec<Arg>,
}

fn to_u32(n: usize) -> u32 {
    // Over 4G arguments would need over 32 GB for the arguments alone.
    u32::try_from(n).expect("finding argument buffer exceeds u32::MAX entries")
}

impl Findings {
    /// An empty buffer.
    pub fn new() -> Findings {
        Findings::default()
    }

    /// An empty buffer with room for `findings` findings and `args` arguments.
    pub fn with_capacity(findings: usize, args: usize) -> Findings {
        Findings { items: Vec::with_capacity(findings), args: Vec::with_capacity(args) }
    }

    /// Records a finding. `confidence` is clamped to 100. In debug builds, panics if the
    /// arguments do not match the rule's declared [`ArgKind`]s or a rule with
    /// [`ConfidenceModel::Certain`](super::ConfidenceModel::Certain) gets less than 100.
    #[inline]
    pub fn push(&mut self, rule: Rule, loc: Location, symbol: SymbolId, confidence: u8, args: &[Arg]) {
        self.push_full(rule, loc, 0, symbol, confidence, 0, args, &[]);
    }

    /// [`push`](Self::push) with confidence reasons ([`Finding::reasons`]).
    #[inline]
    pub fn push_with_reasons(&mut self, rule: Rule, loc: Location, symbol: SymbolId, confidence: u8, args: &[Arg], reasons: u32) {
        self.push_full(rule, loc, 0, symbol, confidence, reasons, args, &[]);
    }

    /// [`push`](Self::push) with the last line of the reported span (`0` = unknown) and extra
    /// data-only arguments (`extra`, not rendered in the message nor part of baseline keys;
    /// see [`extra`](Self::extra)).
    #[allow(clippy::too_many_arguments)]
    #[inline]
    pub fn push_ext(
        &mut self,
        rule: Rule,
        loc: Location,
        end_line: u32,
        symbol: SymbolId,
        confidence: u8,
        args: &[Arg],
        extra: &[Arg],
    ) {
        self.push_full(rule, loc, end_line, symbol, confidence, 0, args, extra);
    }

    /// Every field of a finding: [`push_ext`](Self::push_ext) with confidence reasons.
    #[allow(clippy::too_many_arguments)]
    #[inline]
    pub fn push_full(
        &mut self,
        rule: Rule,
        loc: Location,
        end_line: u32,
        symbol: SymbolId,
        confidence: u8,
        reasons: u32,
        args: &[Arg],
        extra: &[Arg],
    ) {
        #[cfg(debug_assertions)]
        {
            let def = rule.def();
            assert!(def.accepts(args), "{rule}: arguments {args:?} do not match {:?}", def.args);
            if def.confidence == super::ConfidenceModel::Certain {
                assert!(confidence >= 100, "{rule}: certain rule with confidence {confidence}");
            }
        }
        let args_start = to_u32(self.args.len());
        self.args.extend_from_slice(args);
        self.args.extend_from_slice(extra);
        self.items.push(Finding {
            rule,
            confidence: confidence.min(100),
            loc,
            end_line,
            symbol,
            reasons,
            args_start,
            args_len: to_u32(args.len()),
            extra_len: to_u32(extra.len()),
        });
    }

    /// Moves every finding of `other` to the end of `self` (concatenation; argument offsets are
    /// rebased). `other` is left empty with its capacity, ready for reuse.
    pub fn append(&mut self, other: &mut Findings) {
        let base = to_u32(self.args.len());
        // Rebased offsets must stay addressable.
        let _ = to_u32(self.args.len() + other.args.len());
        self.args.append(&mut other.args);
        self.items.reserve(other.items.len());
        for mut f in other.items.drain(..) {
            f.args_start += base;
            self.items.push(f);
        }
    }

    /// Number of findings.
    #[inline]
    pub fn len(&self) -> usize {
        self.items.len()
    }

    /// Whether there are none.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Removes every finding, keeping capacity.
    pub fn clear(&mut self) {
        self.items.clear();
        self.args.clear();
    }

    /// The findings, in their current order.
    #[inline]
    pub fn as_slice(&self) -> &[Finding] {
        &self.items
    }

    /// The arguments of a finding of this buffer.
    #[inline]
    pub fn args(&self, f: &Finding) -> &[Arg] {
        &self.args[f.args_start as usize..(f.args_start + f.args_len) as usize]
    }

    /// The extra (data-only) arguments of a finding of this buffer (empty for most rules).
    #[inline]
    pub fn extra(&self, f: &Finding) -> &[Arg] {
        let s = (f.args_start + f.args_len) as usize;
        &self.args[s..s + f.extra_len as usize]
    }

    /// Findings with their arguments.
    pub fn iter(&self) -> impl ExactSizeIterator<Item = (&Finding, &[Arg])> {
        self.items.iter().map(|f| (f, self.args(f)))
    }

    /// Sorts findings (arguments stay where they are; only the records move).
    pub fn sort_by(&mut self, cmp: impl FnMut(&Finding, &Finding) -> Ordering) {
        self.items.sort_unstable_by(cmp);
    }

    /// Sorts by file id, line, column, then rule. Output sorts by path instead, which needs the
    /// file table.
    pub fn sort_by_location(&mut self) {
        self.items.sort_unstable_by_key(|f| (f.loc, f.rule));
    }

    /// Keeps the findings for which `keep` returns true, preserving order, and compacts the
    /// argument buffer in place. Allocation-free unless the findings were sorted since they
    /// were pushed (then the arguments are first re-laid out in finding order).
    pub fn retain(&mut self, mut keep: impl FnMut(&Finding, &[Arg]) -> bool) {
        if !self.args_in_order() {
            self.rebuild_args();
        }
        let mut write = 0usize;
        let mut args_write = 0usize;
        for read in 0..self.items.len() {
            let f = self.items[read];
            let (s, n) = (f.args_start as usize, f.args_len as usize);
            let total = n + f.extra_len as usize;
            if !keep(&f, &self.args[s..s + n]) {
                continue;
            }
            // Arguments are in finding order, so `s >= args_write`: copying never clobbers
            // arguments of a later finding.
            self.args.copy_within(s..s + total, args_write);
            self.items[write] = Finding { args_start: to_u32(args_write), ..f };
            write += 1;
            args_write += total;
        }
        self.items.truncate(write);
        self.args.truncate(args_write);
    }

    /// Whether each finding's arguments start where the previous finding's end.
    fn args_in_order(&self) -> bool {
        let mut next = 0u32;
        for f in &self.items {
            if f.args_start != next {
                return false;
            }
            next += f.args_len + f.extra_len;
        }
        true
    }

    /// Re-lays the argument buffer out in finding order.
    fn rebuild_args(&mut self) {
        let mut args = Vec::with_capacity(self.args.len());
        for f in &mut self.items {
            let s = f.args_start as usize;
            let start = to_u32(args.len());
            args.extend_from_slice(&self.args[s..s + (f.args_len + f.extra_len) as usize]);
            f.args_start = start;
        }
        self.args = args;
    }

    /// Writes the message of a finding of this buffer.
    pub fn write_message(&self, f: &Finding, r: &dyn ArgResolver, out: &mut dyn fmt::Write) -> fmt::Result {
        f.rule.def().write_message(self.args(f), r, out)
    }
}

/// A malformed message template (found by the registry tests).
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum MessageError {
    /// `{` without a valid `N}` / `N..SEP}` after it, or a stray `}`.
    #[error("malformed placeholder at byte {0}")]
    Syntax(usize),
    /// A placeholder refers to an argument the rule does not declare.
    #[error("placeholder {{{0}}} has no declared argument")]
    Missing(usize),
    /// A `{N..SEP}` placeholder that is not the last placeholder or not for the last argument.
    #[error("`..` placeholder must be the last placeholder and refer to the last argument")]
    Rest,
    /// A declared argument that no placeholder uses.
    #[error("argument {0} is never used by the message")]
    Unused(usize),
}

/// A piece of a message template.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Piece<'a> {
    /// Literal text.
    Lit(&'a str),
    /// Argument N.
    Arg(usize),
    /// Arguments N.. joined by the separator.
    Rest(usize, &'a str),
}

/// Splits a template into pieces. Placeholders: `{N}` and `{N..SEP}`, N a decimal index (at
/// most two digits). Literal text may not contain `{` or `}`.
pub(crate) struct Pieces<'a> {
    rest: &'a str,
    pos: usize,
}

pub(crate) fn pieces(template: &str) -> Pieces<'_> {
    Pieces { rest: template, pos: 0 }
}

impl<'a> Pieces<'a> {
    fn fail(&mut self, at: usize) -> Option<Result<Piece<'a>, MessageError>> {
        self.rest = "";
        Some(Err(MessageError::Syntax(at)))
    }
}

impl<'a> Iterator for Pieces<'a> {
    type Item = Result<Piece<'a>, MessageError>;

    fn next(&mut self) -> Option<Self::Item> {
        let at = self.pos;
        match self.rest.as_bytes().first()? {
            b'{' => {}
            b'}' => return self.fail(at),
            _ => {
                let end = self.rest.find(['{', '}']).unwrap_or(self.rest.len());
                let (lit, tail) = self.rest.split_at(end);
                self.rest = tail;
                self.pos += end;
                return Some(Ok(Piece::Lit(lit)));
            }
        }
        let Some(close) = self.rest.find('}') else { return self.fail(at) };
        let inner = &self.rest[1..close];
        let digits = inner.bytes().take_while(u8::is_ascii_digit).count();
        if digits == 0 || digits > 2 {
            return self.fail(at);
        }
        let index = inner[..digits].bytes().fold(0usize, |acc, b| acc * 10 + usize::from(b - b'0'));
        let piece = match &inner[digits..] {
            "" => Piece::Arg(index),
            tail => match tail.strip_prefix("..") {
                Some(sep) if !sep.contains('{') => Piece::Rest(index, sep),
                _ => return self.fail(at),
            },
        };
        self.rest = &self.rest[close + 1..];
        self.pos += close + 1;
        Some(Ok(piece))
    }
}

impl RuleDef {
    /// Whether the template uses a `{N..SEP}` placeholder (the last argument kind repeats).
    pub fn is_variadic(&self) -> bool {
        pieces(self.message).any(|p| matches!(p, Ok(Piece::Rest(..))))
    }

    /// Whether `args` matches the declared kinds: exactly, or for variadic rules with the last
    /// kind repeated one or more times.
    pub fn accepts(&self, args: &[Arg]) -> bool {
        let kinds = self.args;
        let prefix_ok = |n: usize| args.iter().zip(&kinds[..n]).all(|(a, &k)| a.fits(k));
        if self.is_variadic() {
            let Some((&last, head)) = kinds.split_last() else { return false };
            args.len() >= kinds.len() && prefix_ok(head.len()) && args[head.len()..].iter().all(|a| a.fits(last))
        } else {
            args.len() == kinds.len() && prefix_ok(kinds.len())
        }
    }

    /// Checks the template against the declared arguments: placeholders parse and refer to
    /// declared arguments, a `..` placeholder comes last and covers the last argument, and every
    /// argument is used.
    pub fn check_message(&self) -> Result<(), MessageError> {
        let n = self.args.len();
        let mut used = [false; 16];
        if n > used.len() {
            return Err(MessageError::Missing(used.len()));
        }
        let mut seen_rest = false;
        for piece in pieces(self.message) {
            match piece? {
                Piece::Lit(_) => {}
                Piece::Arg(_) | Piece::Rest(..) if seen_rest => return Err(MessageError::Rest),
                Piece::Arg(i) if i >= n => return Err(MessageError::Missing(i)),
                Piece::Arg(i) => used[i] = true,
                Piece::Rest(i, _) if i >= n => return Err(MessageError::Missing(i)),
                Piece::Rest(i, _) if i + 1 != n => return Err(MessageError::Rest),
                Piece::Rest(i, _) => {
                    used[i] = true;
                    seen_rest = true;
                }
            }
        }
        match used[..n].iter().position(|u| !u) {
            Some(i) => Err(MessageError::Unused(i)),
            None => Ok(()),
        }
    }

    /// Writes the message for `args`, which should satisfy [`accepts`](Self::accepts) (missing
    /// arguments render as `?`). No allocation: numbers are formatted directly into `out`, ids
    /// through `r`.
    pub fn write_message(&self, args: &[Arg], r: &dyn ArgResolver, out: &mut dyn fmt::Write) -> fmt::Result {
        for piece in pieces(self.message) {
            match piece {
                Ok(Piece::Lit(s)) => out.write_str(s)?,
                Ok(Piece::Arg(i)) => match (args.get(i), self.args.get(i)) {
                    (Some(&Arg::Phrase(p)), Some(ArgKind::Phrase(words))) => out.write_str(words.get(p as usize).unwrap_or(&"?"))?,
                    (Some(&a), _) => write_arg(a, r, out)?,
                    (None, _) => out.write_str("?")?,
                },
                Ok(Piece::Rest(i, sep)) => {
                    for (k, &a) in args.iter().skip(i).enumerate() {
                        if k > 0 {
                            out.write_str(sep)?;
                        }
                        write_arg(a, r, out)?;
                    }
                }
                // Templates are validated by the registry tests; never reached for them.
                Err(_) => break,
            }
        }
        Ok(())
    }
}

fn write_arg(a: Arg, r: &dyn ArgResolver, out: &mut dyn fmt::Write) -> fmt::Result {
    match a {
        Arg::U32(v) => write!(out, "{v}"),
        Arg::F32(v) => write!(out, "{v:.1}"),
        Arg::Symbol(id) => r.write_symbol(id, out),
        Arg::Name(id) => r.write_name(id, out),
        Arg::File(id) => r.write_file(id, out),
        Arg::Contract(i) => r.write_contract(i, out),
        // Rendered from the rule's wordings by `write_message`; not variadic.
        Arg::Phrase(_) => out.write_str("?"),
    }
}

/// Names of the reason bits of `f` ([`Finding::reasons`]), lowest bit first.
pub fn reason_names(f: &Finding) -> impl Iterator<Item = &'static str> {
    let bits = f.reasons;
    let names = super::dead_code::reason::NAMES;
    (0..names.len()).filter(move |&i| bits & (1 << i) != 0).map(move |i| names[i])
}
