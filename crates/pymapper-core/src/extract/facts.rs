//! FileFacts: per-file struct-of-arrays fact tables (design: Front end).
//!
//! [`extract`] runs the parser once over a file's bytes with a fact-building
//! visitor and appends one row per fact to the tables of a [`FactStore`]. The
//! store is per thread and reused across files; the returned [`FileFacts`] is a
//! small header of row ranges into it. Nothing is allocated per fact: every
//! column is a `Vec` that only grows, and the visitor's scratch buffers live in
//! the store too.
//!
//! # Strings
//!
//! Facts never hold string data. Every name is a [`Sym`] handed out by a
//! [`StrSink`] when the fact is recorded:
//!
//! - [`SpanStrs`] (the default before interning): `Sym` indexes a per-file
//!   table of byte spans into the source, so the source must outlive the use of
//!   the facts.
//! - [`Interner`](crate::intern::Interner) (implements [`StrSink`]): `Sym` is an
//!   interned id, so the source can be dropped right after extraction. Per-thread
//!   interners are merged with `Interner::merge_all`, and each worker's facts are
//!   then rewritten with [`FactStore::map_syms`] (or `Remap::apply` directly on the
//!   `Sym` columns).
//!
//! Span facts can also be interned afterwards: [`FactStore::map_syms`] with a
//! closure that interns `spans.text(src, sym)`.
//!
//! # Indices
//!
//! All references *between* rows are file-relative: a scope is the row of its
//! definition counted from the file's first def row (row 0 is the `<module>`
//! pseudo-scope), and a [`Run`] in a row (a name chain, a def's decorators, ...)
//! counts from the file's first row of the referenced table. So merging
//! per-thread stores is plain concatenation ([`FactStore::append`]) plus shifting
//! the per-file headers ([`FileFacts::shift`]); no row is touched.
//!
//! # Layout
//!
//! A chain is a `Sym` column plus a `u8` length column ([`ChainCol`]): one name is
//! stored inline, longer chains point into `names`. Row sizes (asserted in tests):
//!
//! ```text
//!   FactStore (one per thread)
//!   ├── defs        kind u8 | flags u16 | name Option<Sym> | parent u32 | start,end u32 |
//!   │               start_line,end_line u32 | name_line,name_col u32 |
//!   │               decorators u32+u8 | bases u32+u8 | params u32+u16          51 B/row
//!   ├── decorators  chain Sym+u8 | flags u8                                     6 B/row
//!   ├── bases       chain Sym+u8 | keyword Option<Sym> | flags u8              10 B/row
//!   ├── params      name Sym | kind+flags u8                                    5 B/row
//!   ├── imports     scope u32 | module Sym+u8 | name,alias Option<Sym> |
//!   │               level u8 | flags u8 | line,col u32                         27 B/row
//!   ├── calls       scope u32 | chain Sym+u8 | receiver u8 | flags u8 |
//!   │               line,col u32                                               19 B/row
//!   ├── bindings    scope u32 | target Sym+u8 | kind u8 | flags u8 | value u8 |
//!   │               value_chain Sym+u8 | line u32                              21 B/row
//!   ├── decls       scope u32 | kind u8 | name Sym                              9 B/row
//!   ├── names       Sym           chains of 2+ names                            4 B/row
//!   ├── exports     Sym           `__all__` contents                            4 B/row
//!   ├── evidence    u32           sorted unique name hashes per file            4 B/row
//!   ├── diags       kind u8 | line,col u32                                      9 B/row
//!   └── suppressions line u32 | codes Run | all u8                             13 B/row
//!   FileFacts (one per file): a Run per table + line count + flags            112 B
//! ```
//!
//! Chains are also deduplicated by token position: `from m.n import a, b` stores
//! `m.n` once, and `x = Foo()` / `@app.route(...)` reuse the chain of the call
//! event just before them.
//!
//! Measured on the stdlib (437k LOC): 13.4 B/LOC of rows (calls 3.6, bindings
//! 3.2, defs 3.0, names 1.3, evidence 1.1, the rest 1.2) plus 0.27 B/LOC of
//! headers; see "Facts (S1.3)" in `docs/benchmarks.md` (also the venv).
//!
//! # What is recorded
//!
//! - Every `def`/`class`, with decorators, bases/keywords, parameters and the
//!   flags in [`def_flags`]. Metrics are a separate table keyed by def row (S1.4).
//! - Every imported name, with [`import_flags`]. Positions are those of the
//!   imported name (the module for plain `import`).
//! - Every call whose callee is a name chain, possibly after `self`/`cls`/`super()`
//!   or an unknown receiver (see [`Receiver`]); `scope` is the enclosing def
//!   (lambdas and comprehensions belong to it).
//! - Every assignment target that is a name or dotted chain, with the value
//!   shape: `Call`/`Name` keep the value chain (construction / alias hints).
//!   Unpacked targets get [`ValueKind::Other`]. Context flags of header targets
//!   (`for` targets, walrus in a condition, `except ... as e`) are those of the
//!   statement header.
//! - Module-level `__all__` contents ([`file_flags`] says whether it is complete).
//! - `global`/`nonlocal` names.
//! - Evidence: hashes of identifiers used in expressions, attribute names and the
//!   identifier-like segments of plain string literals (`"pkg.mod.func"` gives
//!   `pkg`, `mod`, `func`). Bare names are included (in addition to the design's
//!   attributes and strings) so that a function passed as a value
//!   (`register(handler)`) has evidence too.
//! - Diagnostics and `# pymapper: ignore[...]` suppressions with their lines.

mod build;
mod render;
mod strs;
#[cfg(test)]
mod tests;

pub use render::render;
pub use strs::{SpanStrs, StrSink};

use super::events::{AssignKind, DeclKind, DiagKind, ParamKind, Receiver};
use crate::intern::Sym;

// ---------------------------------------------------------------------------------------------
// Basic types
// ---------------------------------------------------------------------------------------------

/// A run of rows `start..start + len` of some table.
///
/// Inside a row, runs are file-relative (count from the file's first row of the
/// referenced table). In a [`FileFacts`] header they are store-absolute.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub struct Run {
    pub start: u32,
    pub len: u32,
}

impl Run {
    pub const EMPTY: Run = Run { start: 0, len: 0 };

    #[inline]
    pub fn range(self) -> std::ops::Range<usize> {
        self.start as usize..self.start as usize + self.len as usize
    }

    #[inline]
    pub fn is_empty(self) -> bool {
        self.len == 0
    }

    #[inline]
    fn shift(self, base: u32) -> Run {
        Run { start: self.start + base, len: self.len }
    }
}

/// File-relative def row (`0` = `<module>`).
pub type Scope = u32;

/// Parent of the `<module>` row.
pub const NO_PARENT: u32 = u32::MAX;

/// Kind of a def row.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum DefKind {
    /// The `<module>` pseudo-scope (always row 0 of a file).
    Module = 0,
    Function = 1,
    Class = 2,
}

impl DefKind {
    #[inline]
    pub fn from_u8(v: u8) -> DefKind {
        match v {
            1 => DefKind::Function,
            2 => DefKind::Class,
            _ => DefKind::Module,
        }
    }
}

/// Def flags (`u16` column).
pub mod def_flags {
    pub const ASYNC: u16 = 1 << 0;
    /// `@property`, `@cached_property`, `@x.setter` / `.getter` / `.deleter`.
    pub const PROPERTY: u16 = 1 << 1;
    pub const STATICMETHOD: u16 = 1 << 2;
    pub const CLASSMETHOD: u16 = 1 << 3;
    /// `@overload` / `@typing.overload`.
    pub const OVERLOAD: u16 = 1 << 4;
    /// Name starts with `_` and is not a dunder.
    pub const PRIVATE: u16 = 1 << 5;
    /// `__name__`.
    pub const DUNDER: u16 = 1 << 6;
    /// `@abstractmethod` (any dotted form).
    pub const ABSTRACT: u16 = 1 << 7;
    /// Defined directly in a class body.
    pub const METHOD: u16 = 1 << 8;
    /// Defined inside a function (nested function or class).
    pub const NESTED: u16 = 1 << 9;
    /// Defined inside an `if TYPE_CHECKING:` block.
    pub const TYPE_CHECKING: u16 = 1 << 10;
    /// Defined inside a conditional block (if/elif/else, loop, try family, match case).
    pub const CONDITIONAL: u16 = 1 << 11;
    /// Function has a `-> ...` return annotation.
    pub const RETURN_ANNOTATION: u16 = 1 << 12;
    /// Has decorators other than the recognised ones above.
    pub const OTHER_DECORATOR: u16 = 1 << 13;
}

/// Decorator flags.
pub mod dec_flags {
    /// The chain is followed by a call (`@app.route("/")`).
    pub const CALLED: u8 = 1 << 0;
    /// Not a plain dotted name (chain empty).
    pub const COMPLEX: u8 = 1 << 1;
}

/// Base flags.
pub mod base_flags {
    pub const SUBSCRIPTED: u8 = 1 << 0;
    pub const STARRED: u8 = 1 << 1;
}

/// Param `kind` (low 3 bits: [`ParamKind`] as `u8`) and flags.
pub mod param_flags {
    pub const KIND_MASK: u8 = 0b111;
    pub const DEFAULT: u8 = 1 << 3;
    pub const ANNOTATED: u8 = 1 << 4;
}

#[inline]
pub(crate) fn param_kind_u8(k: ParamKind) -> u8 {
    match k {
        ParamKind::PositionalOnly => 0,
        ParamKind::Normal => 1,
        ParamKind::VarPositional => 2,
        ParamKind::KeywordOnly => 3,
        ParamKind::VarKeyword => 4,
    }
}

/// Inverse of the `kind` bits of a param row.
#[inline]
pub fn param_kind(v: u8) -> ParamKind {
    match v & param_flags::KIND_MASK {
        0 => ParamKind::PositionalOnly,
        2 => ParamKind::VarPositional,
        3 => ParamKind::KeywordOnly,
        4 => ParamKind::VarKeyword,
        _ => ParamKind::Normal,
    }
}

/// Import flags.
pub mod import_flags {
    /// `from ... import` form.
    pub const FROM: u8 = 1 << 0;
    /// `from m import *`.
    pub const STAR: u8 = 1 << 1;
    /// Inside `if TYPE_CHECKING:` (or the `else` of `if not TYPE_CHECKING:`).
    pub const TYPE_CHECKING: u8 = 1 << 2;
    /// Inside the body of a `try` that has an `except ImportError` /
    /// `except ModuleNotFoundError` handler (also tuples containing them), or
    /// inside such a handler.
    pub const TRY_IMPORT_ERROR: u8 = 1 << 3;
    /// Inside a conditional block (if/elif/else, loop, try family, match case) of its own
    /// scope (blocks around an enclosing def do not count).
    pub const CONDITIONAL: u8 = 1 << 4;
}

/// Call flags.
pub mod call_flags {
    /// Inside the module-level `if __name__ == "__main__":` block (scope `<module>`; calls in
    /// defs nested in the block are not flagged).
    pub const MAIN_GUARD: u8 = 1 << 0;
    /// Inside `if TYPE_CHECKING:` (also through enclosing defs).
    pub const TYPE_CHECKING: u8 = 1 << 1;
    /// Inside a lambda (scope is the lambda's enclosing def).
    pub const LAMBDA: u8 = 1 << 2;
}

#[inline]
pub(crate) fn receiver_u8(r: Receiver) -> u8 {
    match r {
        Receiver::None => 0,
        Receiver::Dotted => 1,
        Receiver::SelfAttr => 2,
        Receiver::ClsAttr => 3,
        Receiver::Super => 4,
        Receiver::Unknown => 5,
    }
}

/// Inverse of the receiver column.
#[inline]
pub fn receiver(v: u8) -> Receiver {
    match v {
        1 => Receiver::Dotted,
        2 => Receiver::SelfAttr,
        3 => Receiver::ClsAttr,
        4 => Receiver::Super,
        5 => Receiver::Unknown,
        _ => Receiver::None,
    }
}

/// Binding flags.
pub mod binding_flags {
    /// One element of an unpacking target.
    pub const UNPACKED: u8 = 1 << 0;
    /// `*rest` target.
    pub const STARRED: u8 = 1 << 1;
    pub const TYPE_CHECKING: u8 = 1 << 2;
    pub const CONDITIONAL: u8 = 1 << 3;
}

#[inline]
pub(crate) fn assign_kind_u8(k: AssignKind) -> u8 {
    match k {
        AssignKind::Plain => 0,
        AssignKind::Augmented => 1,
        AssignKind::Annotated => 2,
        AssignKind::Walrus => 3,
        AssignKind::ForTarget => 4,
        AssignKind::AsTarget => 5,
    }
}

/// Inverse of the binding `kind` column.
#[inline]
pub fn assign_kind(v: u8) -> AssignKind {
    match v {
        1 => AssignKind::Augmented,
        2 => AssignKind::Annotated,
        3 => AssignKind::Walrus,
        4 => AssignKind::ForTarget,
        5 => AssignKind::AsTarget,
        _ => AssignKind::Plain,
    }
}

/// Shape of a bound value (binding `value` column).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum ValueKind {
    /// No value (`x: int`, for/as targets).
    None = 0,
    /// `Foo(...)` / `m.Foo(...)`: `value_chain` is the callee chain (a construction hint).
    Call = 1,
    /// A plain dotted name: `value_chain` is the name (an alias hint).
    Name = 2,
    /// A list/tuple/set display of plain strings (contents only kept for `__all__`).
    Strings = 3,
    Other = 4,
}

impl ValueKind {
    #[inline]
    pub fn from_u8(v: u8) -> ValueKind {
        match v {
            0 => ValueKind::None,
            1 => ValueKind::Call,
            2 => ValueKind::Name,
            3 => ValueKind::Strings,
            _ => ValueKind::Other,
        }
    }
}

#[inline]
pub(crate) fn decl_kind_u8(k: DeclKind) -> u8 {
    match k {
        DeclKind::Global => 0,
        DeclKind::Nonlocal => 1,
    }
}

/// Inverse of the decl `kind` column.
#[inline]
pub fn decl_kind(v: u8) -> DeclKind {
    if v == 1 { DeclKind::Nonlocal } else { DeclKind::Global }
}

#[inline]
pub(crate) fn diag_kind_u8(k: DiagKind) -> u8 {
    match k {
        DiagKind::Lex => 0,
        DiagKind::UnexpectedToken => 1,
        DiagKind::ExpectedColon => 2,
        DiagKind::ExpectedIndent => 3,
        DiagKind::UnexpectedIndent => 4,
        DiagKind::UnclosedBracket => 5,
        DiagKind::UnmatchedBracket => 6,
        DiagKind::ExpectedName => 7,
        DiagKind::Python2 => 8,
    }
}

/// Inverse of the diagnostic `kind` column.
#[inline]
pub fn diag_kind(v: u8) -> DiagKind {
    match v {
        0 => DiagKind::Lex,
        1 => DiagKind::UnexpectedToken,
        2 => DiagKind::ExpectedColon,
        3 => DiagKind::ExpectedIndent,
        4 => DiagKind::UnexpectedIndent,
        5 => DiagKind::UnclosedBracket,
        6 => DiagKind::UnmatchedBracket,
        7 => DiagKind::ExpectedName,
        _ => DiagKind::Python2,
    }
}

/// File flags.
pub mod file_flags {
    /// Has a module-level `if __name__ == "__main__":` block.
    pub const MAIN_GUARD: u8 = 1 << 0;
    /// Assigns `__all__` at module level.
    pub const ALL_DEFINED: u8 = 1 << 1;
    /// `__all__` is not fully known: assigned/extended with a value that is not
    /// a list/tuple of plain strings, or mutated by a method call
    /// (`__all__.extend(...)`). Conditional bindings are unioned.
    pub const ALL_DYNAMIC: u8 = 1 << 2;
    /// The source exceeded `lex::MAX_SOURCE_LEN` and was truncated.
    pub const TRUNCATED: u8 = 1 << 3;
}

// ---------------------------------------------------------------------------------------------
// Tables
// ---------------------------------------------------------------------------------------------

/// Defines a struct-of-arrays table: one `Vec` per column, `push` taking the
/// columns positionally, plus `len`, `clear`, `append`, `heap_bytes` and
/// `row_bytes` (bytes of the stored rows, ignoring spare capacity).
macro_rules! soa {
    ($(#[$m:meta])* $name:ident { $($(#[$fm:meta])* $f:ident : $t:ty),+ $(,)? }) => {
        $(#[$m])*
        #[derive(Default, Debug)]
        pub struct $name {
            $($(#[$fm])* pub $f: Vec<$t>,)+
        }

        impl $name {
            /// Bytes of one row.
            pub const ROW_BYTES: usize = 0 $(+ std::mem::size_of::<$t>())+;

            /// Number of rows.
            #[inline]
            pub fn len(&self) -> u32 {
                soa!(@first self $($f)+).len() as u32
            }

            #[inline]
            pub fn is_empty(&self) -> bool {
                self.len() == 0
            }

            /// Appends a row and returns its store-absolute index.
            #[inline]
            #[allow(clippy::too_many_arguments)]
            pub fn push(&mut self, $($f: $t),+) -> u32 {
                let i = self.len();
                $(self.$f.push($f);)+
                i
            }

            pub fn clear(&mut self) {
                $(self.$f.clear();)+
            }

            /// Moves all rows of `other` to the end; returns the base to shift by.
            fn shrink_to_fit(&mut self) {
                $(self.$f.shrink_to_fit();)+
            }

            fn append(&mut self, other: &mut Self) -> u32 {
                let base = self.len();
                $(self.$f.append(&mut other.$f);)+
                base
            }

            /// Heap bytes reserved (including spare capacity).
            pub fn heap_bytes(&self) -> usize {
                0 $(+ self.$f.capacity() * std::mem::size_of::<$t>())+
            }

            /// Bytes of the stored rows.
            pub fn row_bytes(&self) -> usize {
                self.len() as usize * Self::ROW_BYTES
            }
        }
    };
    (@first $s:ident $f:ident $($rest:ident)*) => { $s.$f };
}

soa! {
    /// Definitions: the `<module>` pseudo-scope (row 0 of each file), functions and classes, in
    /// source order of their headers (pre-order).
    Defs {
        /// [`DefKind`] as `u8`.
        kind: u8,
        /// [`def_flags`].
        flags: u16,
        /// `None` for `<module>`.
        name: Option<Sym>,
        /// File-relative parent def row ([`NO_PARENT`] for `<module>`).
        parent: u32,
        /// Byte span: first decorator's `@` (else `async`, else the keyword) to the end of the
        /// last token of the body.
        start: u32,
        end: u32,
        start_line: u32,
        end_line: u32,
        /// Position of the name token (1-based line, 0-based byte column).
        name_line: u32,
        name_col: u32,
        /// File-relative first row of [`Decorators`] (see [`Defs::decorators_run`]).
        decorators: u32,
        decorators_len: u8,
        /// File-relative first row of [`Bases`] (classes; see [`Defs::bases_run`]).
        bases: u32,
        bases_len: u8,
        /// File-relative first row of [`Params`] (functions; see [`Defs::params_run`]).
        params: u32,
        params_len: u16,
    }
}

impl Defs {
    /// File-relative decorator rows of row `i`.
    #[inline]
    pub fn decorators_run(&self, i: usize) -> Run {
        Run { start: self.decorators[i], len: self.decorators_len[i] as u32 }
    }

    /// File-relative base rows of row `i`.
    #[inline]
    pub fn bases_run(&self, i: usize) -> Run {
        Run { start: self.bases[i], len: self.bases_len[i] as u32 }
    }

    /// File-relative param rows of row `i`.
    #[inline]
    pub fn params_run(&self, i: usize) -> Run {
        Run { start: self.params[i], len: self.params_len[i] as u32 }
    }
}

soa! {
    /// Decorators, grouped per def (see [`Defs::decorators`]).
    Decorators {
        /// Name chain ([`ChainCol`] encoding; empty when not a plain dotted name).
        chain: Sym,
        chain_len: u8,
        /// [`dec_flags`].
        flags: u8,
    }
}

soa! {
    /// Class bases and class keywords (`metaclass=M`), grouped per class.
    Bases {
        /// Name chain ([`ChainCol`] encoding; empty for other expressions).
        chain: Sym,
        chain_len: u8,
        /// `Some(kw)` for a keyword argument.
        keyword: Option<Sym>,
        /// [`base_flags`].
        flags: u8,
    }
}

soa! {
    /// Function parameters, grouped per def.
    Params {
        name: Sym,
        /// [`param_flags`]: kind in the low bits.
        flags: u8,
    }
}

soa! {
    /// One row per imported name (`from m import a, b` is two rows).
    Imports {
        scope: Scope,
        /// The dotted module ([`ChainCol`] encoding; `from . import x` -> empty).
        module: Sym,
        module_len: u8,
        /// `from` form: the imported name (`None` for `*` and plain `import`).
        name: Option<Sym>,
        alias: Option<Sym>,
        /// Relative import level (number of leading dots, saturating at 255).
        level: u8,
        /// [`import_flags`].
        flags: u8,
        line: u32,
        col: u32,
    }
}

soa! {
    /// Call sites.
    Calls {
        scope: Scope,
        /// The callee chain ([`ChainCol`] encoding; see [`Receiver`]).
        chain: Sym,
        chain_len: u8,
        /// [`Receiver`] as `u8` (see [`receiver`]).
        receiver: u8,
        /// [`call_flags`].
        flags: u8,
        /// Position of the first token of the callee chain.
        line: u32,
        col: u32,
    }
}

soa! {
    /// Assignment targets that are plain names or dotted chains (`x`, `self.x`, `a.b`).
    Bindings {
        scope: Scope,
        /// The target chain ([`ChainCol`] encoding).
        target: Sym,
        target_len: u8,
        /// [`AssignKind`] as `u8` (see [`assign_kind`]).
        kind: u8,
        /// [`binding_flags`].
        flags: u8,
        /// [`ValueKind`] as `u8`.
        value: u8,
        /// The value chain for `ValueKind::Call` / `ValueKind::Name` ([`ChainCol`] encoding).
        value_chain: Sym,
        value_len: u8,
        line: u32,
    }
}

soa! {
    /// `global` / `nonlocal` names, one row per name.
    Decls {
        scope: Scope,
        /// [`DeclKind`] as `u8` (see [`decl_kind`]).
        kind: u8,
        name: Sym,
    }
}

soa! {
    /// Parser diagnostics.
    Diags {
        /// [`DiagKind`] as `u8` (see [`diag_kind`]).
        kind: u8,
        line: u32,
        col: u32,
    }
}

soa! {
    /// `# pymapper: ignore[...]` comments.
    Suppressions {
        /// Line the comment is on (it suppresses findings on that line).
        line: u32,
        /// File-relative run of `names`: the listed codes.
        codes: Run,
        /// 1 for a bare `ignore` (every code).
        all: u8,
    }
}

/// Longest name chain stored (longer chains are cut, see the builder).
pub const MAX_CHAIN: usize = u8::MAX as usize;

/// Encoding of a name chain in two columns, a `Sym` column `c` and a `u8` length column:
///
/// - length 0: empty chain (`c` is meaningless);
/// - length 1: `c` is the name itself (most chains are one name: no `names` row is used);
/// - length n >= 2: `c` is `Sym::from_index(start)`, an encoded file-relative start in
///   `names` (not a string id).
///
/// Read chains through [`FileView`] (`call_chain`, `binding_target`, ...), which decode this.
#[derive(Clone, Copy, Debug)]
pub struct ChainCol {
    pub c: Sym,
    pub len: u8,
}

impl Default for ChainCol {
    fn default() -> Self {
        ChainCol::EMPTY
    }
}

impl ChainCol {
    pub const EMPTY: ChainCol = ChainCol { c: Sym::from_index(0), len: 0 };
}

// ---------------------------------------------------------------------------------------------
// Store and per-file header
// ---------------------------------------------------------------------------------------------

/// Per-thread fact tables. Reuse one store for many files; merge stores with
/// [`FactStore::append`].
#[derive(Default, Debug)]
pub struct FactStore {
    pub defs: Defs,
    pub decorators: Decorators,
    pub bases: Bases,
    pub params: Params,
    pub imports: Imports,
    pub calls: Calls,
    pub bindings: Bindings,
    pub decls: Decls,
    /// Name chains referenced by runs in the tables above.
    pub names: Vec<Sym>,
    /// `__all__` contents, per file.
    pub exports: Vec<Sym>,
    /// Evidence hashes per file, sorted and unique (see [`evidence_hash`]).
    pub evidence: Vec<u32>,
    pub diags: Diags,
    pub suppressions: Suppressions,
    /// Reusable scratch of the extractor (not facts).
    scratch: build::Scratch,
}

/// Store-absolute base offsets of every table, returned by [`FactStore::append`]
/// and [`FactStore::bases`]; apply them with [`FileFacts::shift`].
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct TableBases {
    pub defs: u32,
    pub decorators: u32,
    pub bases: u32,
    pub params: u32,
    pub imports: u32,
    pub calls: u32,
    pub bindings: u32,
    pub decls: u32,
    pub names: u32,
    pub exports: u32,
    pub evidence: u32,
    pub diags: u32,
    pub suppressions: u32,
}

/// Header of one file's facts: store-absolute row ranges of every table.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct FileFacts {
    pub defs: Run,
    pub decorators: Run,
    pub bases: Run,
    pub params: Run,
    pub imports: Run,
    pub calls: Run,
    pub bindings: Run,
    pub decls: Run,
    pub names: Run,
    pub exports: Run,
    pub evidence: Run,
    pub diags: Run,
    pub suppressions: Run,
    /// Number of lines (a final line without a line end counts).
    pub lines: u32,
    /// [`file_flags`].
    pub flags: u8,
}

/// Converts a store length to `u32`; tables are limited to `u32::MAX` rows.
#[inline]
fn len32(n: usize) -> u32 {
    u32::try_from(n).unwrap_or_else(|_| panic!("fact table exceeds u32::MAX rows"))
}

impl FactStore {
    pub fn new() -> FactStore {
        FactStore::default()
    }

    /// Current length of every table (where the next file's rows start).
    pub fn bases(&self) -> TableBases {
        TableBases {
            defs: self.defs.len(),
            decorators: self.decorators.len(),
            bases: self.bases.len(),
            params: self.params.len(),
            imports: self.imports.len(),
            calls: self.calls.len(),
            bindings: self.bindings.len(),
            decls: self.decls.len(),
            names: len32(self.names.len()),
            exports: len32(self.exports.len()),
            evidence: len32(self.evidence.len()),
            diags: self.diags.len(),
            suppressions: self.suppressions.len(),
        }
    }

    /// Removes all facts, keeping the allocations.
    pub fn clear(&mut self) {
        self.defs.clear();
        self.decorators.clear();
        self.bases.clear();
        self.params.clear();
        self.imports.clear();
        self.calls.clear();
        self.bindings.clear();
        self.decls.clear();
        self.names.clear();
        self.exports.clear();
        self.evidence.clear();
        self.diags.clear();
        self.suppressions.clear();
    }

    /// Moves every row of `other` to the end of `self` (leaving `other` empty
    /// with its allocations) and returns the bases to shift `other`'s
    /// [`FileFacts`] headers by. Rows are file-relative, so no row changes.
    pub fn append(&mut self, other: &mut FactStore) -> TableBases {
        let b = TableBases {
            defs: self.defs.append(&mut other.defs),
            decorators: self.decorators.append(&mut other.decorators),
            bases: self.bases.append(&mut other.bases),
            params: self.params.append(&mut other.params),
            imports: self.imports.append(&mut other.imports),
            calls: self.calls.append(&mut other.calls),
            bindings: self.bindings.append(&mut other.bindings),
            decls: self.decls.append(&mut other.decls),
            names: len32(self.names.len()),
            exports: len32(self.exports.len()),
            evidence: len32(self.evidence.len()),
            diags: self.diags.append(&mut other.diags),
            suppressions: self.suppressions.append(&mut other.suppressions),
        };
        self.names.append(&mut other.names);
        self.exports.append(&mut other.exports);
        self.evidence.append(&mut other.evidence);
        // Validates the merged lengths.
        let _ = self.bases();
        b
    }

    /// Releases spare capacity of every table (e.g. once a worker is done).
    pub fn shrink_to_fit(&mut self) {
        self.defs.shrink_to_fit();
        self.decorators.shrink_to_fit();
        self.bases.shrink_to_fit();
        self.params.shrink_to_fit();
        self.imports.shrink_to_fit();
        self.calls.shrink_to_fit();
        self.bindings.shrink_to_fit();
        self.decls.shrink_to_fit();
        self.names.shrink_to_fit();
        self.exports.shrink_to_fit();
        self.evidence.shrink_to_fit();
        self.diags.shrink_to_fit();
        self.suppressions.shrink_to_fit();
    }

    /// Bytes of the stored facts (rows only, no spare capacity or scratch).
    pub fn row_bytes(&self) -> usize {
        self.defs.row_bytes()
            + self.decorators.row_bytes()
            + self.bases.row_bytes()
            + self.params.row_bytes()
            + self.imports.row_bytes()
            + self.calls.row_bytes()
            + self.bindings.row_bytes()
            + self.decls.row_bytes()
            + (self.names.len() + self.exports.len() + self.evidence.len()) * 4
            + self.diags.row_bytes()
            + self.suppressions.row_bytes()
    }

    /// Heap bytes reserved by the tables (including spare capacity, excluding scratch).
    pub fn heap_bytes(&self) -> usize {
        self.defs.heap_bytes()
            + self.decorators.heap_bytes()
            + self.bases.heap_bytes()
            + self.params.heap_bytes()
            + self.imports.heap_bytes()
            + self.calls.heap_bytes()
            + self.bindings.heap_bytes()
            + self.decls.heap_bytes()
            + (self.names.capacity() + self.exports.capacity() + self.evidence.capacity()) * 4
            + self.diags.heap_bytes()
            + self.suppressions.heap_bytes()
    }

    /// Rewrites every [`Sym`] of file `ff` (e.g. span ids -> interned ids, or a
    /// worker interner's ids -> merged ids).
    pub fn map_syms(&mut self, ff: &FileFacts, mut f: impl FnMut(Sym) -> Sym) {
        for s in &mut self.names[ff.names.range()] {
            *s = f(*s);
        }
        for s in &mut self.exports[ff.exports.range()] {
            *s = f(*s);
        }
        for s in self.defs.name[ff.defs.range()].iter_mut().flatten() {
            *s = f(*s);
        }
        for s in self.bases.keyword[ff.bases.range()].iter_mut().flatten() {
            *s = f(*s);
        }
        for s in &mut self.params.name[ff.params.range()] {
            *s = f(*s);
        }
        // Inline single names of chain columns.
        let r = ff.decorators.range();
        map_inline(&mut self.decorators.chain[r.clone()], &self.decorators.chain_len[r], &mut f);
        let r = ff.bases.range();
        map_inline(&mut self.bases.chain[r.clone()], &self.bases.chain_len[r], &mut f);
        let r = ff.imports.range();
        map_inline(&mut self.imports.module[r.clone()], &self.imports.module_len[r], &mut f);
        let r = ff.calls.range();
        map_inline(&mut self.calls.chain[r.clone()], &self.calls.chain_len[r], &mut f);
        let r = ff.bindings.range();
        map_inline(&mut self.bindings.target[r.clone()], &self.bindings.target_len[r.clone()], &mut f);
        map_inline(&mut self.bindings.value_chain[r.clone()], &self.bindings.value_len[r], &mut f);
        let r = ff.imports.range();
        for s in self.imports.name[r.clone()].iter_mut().flatten() {
            *s = f(*s);
        }
        for s in self.imports.alias[r].iter_mut().flatten() {
            *s = f(*s);
        }
        for s in &mut self.decls.name[ff.decls.range()] {
            *s = f(*s);
        }
    }

    /// Read access to one file's facts.
    #[inline]
    pub fn file<'a>(&'a self, ff: &'a FileFacts) -> FileView<'a> {
        FileView { store: self, ff }
    }
}

/// Applies `f` to the entries of a [`ChainCol`] column that are inline names (length 1).
fn map_inline(col: &mut [Sym], lens: &[u8], f: &mut impl FnMut(Sym) -> Sym) {
    for (c, &l) in col.iter_mut().zip(lens) {
        if l == 1 {
            *c = f(*c);
        }
    }
}

impl FileFacts {
    /// Rebases the header after its store was appended at `b` (see [`FactStore::append`]).
    pub fn shift(&mut self, b: &TableBases) {
        self.defs = self.defs.shift(b.defs);
        self.decorators = self.decorators.shift(b.decorators);
        self.bases = self.bases.shift(b.bases);
        self.params = self.params.shift(b.params);
        self.imports = self.imports.shift(b.imports);
        self.calls = self.calls.shift(b.calls);
        self.bindings = self.bindings.shift(b.bindings);
        self.decls = self.decls.shift(b.decls);
        self.names = self.names.shift(b.names);
        self.exports = self.exports.shift(b.exports);
        self.evidence = self.evidence.shift(b.evidence);
        self.diags = self.diags.shift(b.diags);
        self.suppressions = self.suppressions.shift(b.suppressions);
    }

    #[inline]
    pub fn has(&self, flag: u8) -> bool {
        self.flags & flag != 0
    }
}

/// One file's facts: resolves file-relative indices against the store.
#[derive(Clone, Copy)]
pub struct FileView<'a> {
    pub store: &'a FactStore,
    pub ff: &'a FileFacts,
}

impl<'a> FileView<'a> {
    /// Number of def rows (including `<module>`).
    #[inline]
    pub fn def_count(&self) -> u32 {
        self.ff.defs.len
    }

    /// Store-absolute row of file-relative def `d`.
    #[inline]
    pub fn def_row(&self, d: Scope) -> usize {
        debug_assert!(d < self.ff.defs.len);
        (self.ff.defs.start + d) as usize
    }

    /// A file-relative run of `names`.
    #[inline]
    pub fn chain(&self, r: Run) -> &'a [Sym] {
        let s = self.ff.names.start as usize;
        &self.store.names[s + r.start as usize..s + r.start as usize + r.len as usize]
    }

    /// Decodes a [`ChainCol`] at store-absolute `row` of `col`/`lens`.
    #[inline]
    pub fn chain_at(&self, col: &'a [Sym], lens: &'a [u8], row: usize) -> &'a [Sym] {
        match lens[row] {
            0 => &[],
            1 => std::slice::from_ref(&col[row]),
            n => {
                let s = self.ff.names.start as usize + col[row].index();
                &self.store.names[s..s + n as usize]
            }
        }
    }

    /// Callee chain of call `row` (store-absolute).
    #[inline]
    pub fn call_chain(&self, row: usize) -> &'a [Sym] {
        self.chain_at(&self.store.calls.chain, &self.store.calls.chain_len, row)
    }

    /// Dotted module of import `row`.
    #[inline]
    pub fn import_module(&self, row: usize) -> &'a [Sym] {
        self.chain_at(&self.store.imports.module, &self.store.imports.module_len, row)
    }

    /// Target chain of binding `row`.
    #[inline]
    pub fn binding_target(&self, row: usize) -> &'a [Sym] {
        self.chain_at(&self.store.bindings.target, &self.store.bindings.target_len, row)
    }

    /// Value chain of binding `row` (`ValueKind::Call` / `ValueKind::Name`).
    #[inline]
    pub fn binding_value(&self, row: usize) -> &'a [Sym] {
        self.chain_at(&self.store.bindings.value_chain, &self.store.bindings.value_len, row)
    }

    /// Chain of decorator `row`.
    #[inline]
    pub fn decorator_chain(&self, row: usize) -> &'a [Sym] {
        self.chain_at(&self.store.decorators.chain, &self.store.decorators.chain_len, row)
    }

    /// Chain of base `row`.
    #[inline]
    pub fn base_chain(&self, row: usize) -> &'a [Sym] {
        self.chain_at(&self.store.bases.chain, &self.store.bases.chain_len, row)
    }

    /// Store-absolute rows of a file-relative run of decorators, bases or params.
    #[inline]
    pub fn rows(&self, table_start: u32, r: Run) -> std::ops::Range<usize> {
        r.shift(table_start).range()
    }

    /// Decorator rows of def `d` (store-absolute).
    #[inline]
    pub fn decorators(&self, d: Scope) -> std::ops::Range<usize> {
        self.rows(self.ff.decorators.start, self.store.defs.decorators_run(self.def_row(d)))
    }

    /// Base rows of def `d` (store-absolute).
    #[inline]
    pub fn bases(&self, d: Scope) -> std::ops::Range<usize> {
        self.rows(self.ff.bases.start, self.store.defs.bases_run(self.def_row(d)))
    }

    /// Param rows of def `d` (store-absolute).
    #[inline]
    pub fn params(&self, d: Scope) -> std::ops::Range<usize> {
        self.rows(self.ff.params.start, self.store.defs.params_run(self.def_row(d)))
    }

    /// `__all__` contents.
    #[inline]
    pub fn exports(&self) -> &'a [Sym] {
        &self.store.exports[self.ff.exports.range()]
    }

    /// Sorted unique evidence hashes.
    #[inline]
    pub fn evidence(&self) -> &'a [u32] {
        &self.store.evidence[self.ff.evidence.range()]
    }

    /// Whether `name` occurs as an identifier, attribute or identifier-like
    /// string in the file (hash match: may give false positives, never false negatives).
    #[inline]
    pub fn has_evidence(&self, name: &[u8]) -> bool {
        self.evidence().binary_search(&evidence_hash(name)).is_ok()
    }
}

/// Hash of a name for the evidence table: the high 32 bits of [`crate::intern::hash_bytes`].
#[inline]
pub fn evidence_hash(name: &[u8]) -> u32 {
    (crate::intern::hash_bytes(name) >> 32) as u32
}

/// Extracts the facts of one file into `store`, getting string ids from `strs`.
///
/// Never fails: malformed input yields diagnostics rows. Inputs longer than
/// [`crate::lex::MAX_SOURCE_LEN`] are truncated (flag [`file_flags::TRUNCATED`]).
pub fn extract<S: StrSink>(src: &[u8], store: &mut FactStore, strs: &mut S) -> FileFacts {
    build::extract(src, store, strs)
}
