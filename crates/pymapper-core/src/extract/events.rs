//! Parser event interface.
//!
//! The parser drives a [`Visitor`]; every method has a default no-op body, so
//! consumers (facts extraction, metrics) implement only what they need and can
//! be developed independently. Visitors are generic parameters of the parser,
//! so unused events compile away.
//!
//! Positions: every event carries [`Token`]s (byte spans into the source the
//! parser was given). Events that facts/metrics usually need a line for also
//! carry a [`Pos`] (1-based line, 0-based byte column).
//!
//! Ordering guarantees:
//! - Events arrive in source order, with one exception: definition headers.
//!   Decorators, default values, annotations and base-class expressions are
//!   evaluated in the *enclosing* scope, so their expression events (calls,
//!   names, ...) and the [`Visitor::decorator`] / [`Visitor::base`] events are
//!   emitted before [`Visitor::def_enter`].
//! - Assignment targets are reported by [`Visitor::assign`] *after* the value's
//!   expression events, so the value shape is known.
//! - `def_enter`/`def_exit`, `block_enter`/`block_exit` and
//!   `lambda_enter`/`lambda_exit` are always balanced, even on malformed input.

use crate::lex::Token;

/// Source position: 1-based line, 0-based byte column.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Pos {
    pub line: u32,
    pub col: u32,
}

/// `def` or `class`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DefKind {
    Function,
    Class,
}

/// A decorator `@a.b.c(...)`. `names` is the dotted chain when the decorator
/// expression is a plain dotted name, optionally called; empty otherwise.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Decorator<'a> {
    /// The `@` token.
    pub at: Token,
    pub names: &'a [Token],
    /// The chain is followed by a call (`@app.route("/")`).
    pub called: bool,
}

/// Parameter kind, as in Python's `inspect.Parameter.kind`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ParamKind {
    /// Before a `/` marker.
    PositionalOnly,
    Normal,
    /// `*args`.
    VarPositional,
    /// After `*` / `*args`.
    KeywordOnly,
    /// `**kwargs`.
    VarKeyword,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Param {
    pub name: Token,
    pub kind: ParamKind,
    pub has_default: bool,
    pub annotated: bool,
}

/// A class base or class keyword (`metaclass=M`). `names` is the dotted chain
/// of a plain dotted-name base (`a.B`, also `a.B[T]` with `subscripted`); empty
/// for other expressions.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Base<'a> {
    pub names: &'a [Token],
    /// `Some(kw)` for a keyword argument (`metaclass=...`).
    pub keyword: Option<Token>,
    pub subscripted: bool,
    /// `*bases` / `**kw`.
    pub starred: bool,
}

/// Everything known about a definition when its body starts.
#[derive(Clone, Copy, Debug)]
pub struct Def<'a> {
    pub kind: DefKind,
    pub name: Token,
    pub name_pos: Pos,
    /// The `def` / `class` keyword.
    pub keyword: Token,
    /// First byte of the definition: first decorator's `@`, else `async`, else the keyword.
    pub start: u32,
    pub start_line: u32,
    pub is_async: bool,
    /// Number of [`Visitor::decorator`] events emitted for this definition (just before it).
    pub decorators: u32,
    /// PEP 695 type parameter names (`def f[T, *Ts, **P]`).
    pub type_params: &'a [Token],
    /// Functions only.
    pub params: &'a [Param],
    /// Classes only: number of [`Visitor::base`] events emitted for this definition (just before it).
    pub bases: u32,
    /// `-> ...` present (functions).
    pub has_return_annotation: bool,
    /// Nesting depth of the enclosing definitions (0 = module level).
    pub depth: u32,
}

/// End of a definition (or block): the end offset and line of its last token.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct End {
    pub end: u32,
    pub line: u32,
}

/// Statement-level block clauses.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BlockKind {
    If,
    Elif,
    /// `else` of an `if`.
    IfElse,
    For,
    While,
    /// `else` of a `for` / `while`.
    LoopElse,
    Try,
    /// `except` / `except*` handler.
    Except,
    /// `else` of a `try`.
    TryElse,
    Finally,
    With,
    Match,
    Case,
}

/// Shape of an `if`/`elif` condition, as far as facts care.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum IfCond {
    Other,
    /// `TYPE_CHECKING` or `<name>.TYPE_CHECKING`.
    TypeChecking,
    /// `not TYPE_CHECKING` / `not <name>.TYPE_CHECKING`.
    NotTypeChecking,
    /// `__name__ == "__main__"` (either operand order, either quote style).
    MainGuard,
}

#[derive(Clone, Copy, Debug)]
pub struct Block<'a> {
    pub kind: BlockKind,
    /// The clause keyword (`if`, `else`, `with`, `case`, ... ; `async` is not included).
    pub keyword: Token,
    pub pos: Pos,
    /// `async for` / `async with`.
    pub is_async: bool,
    /// `If`/`Elif`: condition shape.
    pub cond: IfCond,
    /// `Except`: `except*`.
    pub star: bool,
    /// `Except`: last names of the plain dotted names in the handler's type
    /// expression (`except (a.Error, OSError) as e` -> `[Error, OSError]`).
    /// Empty for a bare `except:`.
    pub except_names: &'a [Token],
    /// `Case`: the pattern is irrefutable (a bare capture/wildcard name, possibly
    /// parenthesised or as an `|` alternative). Same notion as ruff's C901.
    pub irrefutable: bool,
    /// `Case`: has an `if` guard.
    pub guarded: bool,
}

/// Expression-level decision points.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BranchKind {
    /// `a if c else b`; reported at the `if`.
    Ternary,
    /// `for` / `async for` inside a comprehension or generator expression.
    CompFor,
    /// `if` filter inside a comprehension.
    CompIf,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BoolOp {
    And,
    Or,
}

/// What the callee chain of a call hangs off.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Receiver {
    /// `f(...)`: `names == [f]`.
    None,
    /// `a.b.f(...)`: `names == [a, b, f]` (first name is not `self`/`cls`).
    Dotted,
    /// `self.x.f(...)`: `names == [self, x, f]`.
    SelfAttr,
    /// `cls.f(...)`: `names == [cls, f]`.
    ClsAttr,
    /// `super().f(...)` / `super(C, self).f(...)`: `names == [f]` (`names` keeps any further chain, e.g. `super().a.f` -> `[a, f]`).
    Super,
    /// Call on a call result, subscript, literal or parenthesised expression
    /// (`f().g(...)`, `a[0].g(...)`, `"".join(...)`): `names` is the attribute chain after it.
    Unknown,
}

#[derive(Clone, Copy, Debug)]
pub struct Call<'a> {
    pub names: &'a [Token],
    pub receiver: Receiver,
    /// The opening `(`.
    pub lpar: Token,
    /// Position of the first token of the callee expression's chain.
    pub pos: Pos,
}

/// One imported name.
///
/// - `import a.b.c` -> `module = [a, b, c]`, `name = None`, `alias = None`.
/// - `import a.b as m` -> `alias = Some(m)`.
/// - `from ..x.y import n as m` -> `level = 2`, `module = [x, y]`, `name = Some(n)`, `alias = Some(m)`.
/// - `from . import *` -> `level = 1`, `module = []`, `star = true`, `name = None`.
#[derive(Clone, Copy, Debug)]
pub struct Import<'a> {
    pub module: &'a [Token],
    pub name: Option<Token>,
    pub alias: Option<Token>,
    pub level: u32,
    pub star: bool,
    /// `from` form.
    pub from: bool,
    /// The `import` / `from` keyword.
    pub keyword: Token,
    pub pos: Pos,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AssignKind {
    /// `x = v` (also each target of `a = b = v`).
    Plain,
    /// `x += v` etc.; the operator token is in [`Assign::op`].
    Augmented,
    /// `x: T = v` / `x: T`.
    Annotated,
    /// `(x := v)`.
    Walrus,
    /// `for x in ...` loop / comprehension target.
    ForTarget,
    /// `with ... as x` / `except ... as x`.
    AsTarget,
}

/// Shape of an assigned value.
#[derive(Clone, Copy, Debug)]
pub enum Value<'a> {
    /// No value (`x: int`, for/as targets).
    None,
    /// The whole value is one call of a dotted chain: `Foo(...)`, `m.Foo(...)`.
    Call { names: &'a [Token] },
    /// The whole value is a list/tuple/set display of only plain string literals
    /// (`["a", "b"]`, `("a",)`, `[]`): the `__all__` shape.
    Strings(&'a [Token]),
    /// The whole value is a plain dotted name (`x = a.b`).
    Name(&'a [Token]),
    Other,
}

/// One assignment target that is a plain name or dotted attribute chain
/// (`x`, `self.x`, `a.b.c`). Subscript and other complex targets are not reported.
#[derive(Clone, Copy, Debug)]
pub struct Assign<'a> {
    pub kind: AssignKind,
    pub target: &'a [Token],
    /// Target was one element of an unpacking (`a, b = ...`, `for k, v in`).
    pub unpacked: bool,
    /// Starred target (`*rest`).
    pub starred: bool,
    /// Operator token (`=`, `+=`, `:=`, ...); for/as targets: the `for`/`as` keyword.
    pub op: Token,
    pub value: Value<'a>,
    pub pos: Pos,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DeclKind {
    Global,
    Nonlocal,
}

/// Diagnostic codes. The parser never aborts; it reports and resynchronises.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DiagKind {
    /// A lexer `Error*` token.
    Lex,
    /// Token that cannot start or continue the current construct.
    UnexpectedToken,
    /// Expected `:` after a compound statement header.
    ExpectedColon,
    /// Expected an indented block.
    ExpectedIndent,
    /// Indentation where none is allowed.
    UnexpectedIndent,
    /// Bracket left open when a statement keyword appeared.
    UnclosedBracket,
    /// Closing bracket without an opener.
    UnmatchedBracket,
    /// Expected a name (after `def`, `class`, `import`, `as`, ...).
    ExpectedName,
    /// Python 2 syntax (`print x`, `print >>f, x`, `exec code`). Backticks are lexer errors.
    Python2,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Diagnostic {
    pub kind: DiagKind,
    /// Offending token (zero-length at EOF).
    pub token: Token,
    pub pos: Pos,
}

/// Receiver of parser events. All methods default to no-ops.
#[allow(unused_variables)]
pub trait Visitor {
    /// Every token the parser consumes, in order (excluding INDENT/DEDENT/NEWLINE/EOF).
    /// For Halstead-style token metrics.
    #[inline(always)]
    fn token(&mut self, tok: Token) {}

    /// A decorator of the next [`Visitor::def_enter`]; emitted after the decorator's expression events.
    #[inline(always)]
    fn decorator(&mut self, dec: &Decorator<'_>) {}
    /// A base / keyword of the class in the next [`Visitor::def_enter`]; emitted after its expression events.
    #[inline(always)]
    fn base(&mut self, base: &Base<'_>) {}

    /// A `def`/`class` body starts. Header expression events were already emitted.
    #[inline(always)]
    fn def_enter(&mut self, def: &Def<'_>) {}
    #[inline(always)]
    fn def_exit(&mut self, kind: DefKind, end: End) {}

    #[inline(always)]
    fn block_enter(&mut self, block: &Block<'_>) {}
    /// End of the clause opened by the matching `block_enter`. `last_case` is
    /// true for the final `case` of a `match` (only meaningful for `Case`).
    #[inline(always)]
    fn block_exit(&mut self, kind: BlockKind, end: End, last_case: bool) {}

    /// `type X[T] = ...` statement (PEP 695). Value expression events follow.
    #[inline(always)]
    fn type_alias(&mut self, name: Token, type_params: &[Token], pos: Pos) {}

    #[inline(always)]
    fn branch(&mut self, kind: BranchKind, tok: Token) {}

    /// Start of a run of like boolean operators (`a and b and c` is one run,
    /// `a and b or c` two). `tok` is the first operator of the run.
    #[inline(always)]
    fn bool_op(&mut self, op: BoolOp, tok: Token) {}

    #[inline(always)]
    fn lambda_enter(&mut self, tok: Token) {}
    #[inline(always)]
    fn lambda_exit(&mut self, end: u32) {}

    #[inline(always)]
    fn call(&mut self, call: &Call<'_>) {}
    /// `name=` keyword argument in a call.
    #[inline(always)]
    fn keyword_arg(&mut self, name: Token) {}

    #[inline(always)]
    fn import(&mut self, import: &Import<'_>) {}

    #[inline(always)]
    fn assign(&mut self, assign: &Assign<'_>) {}

    #[inline(always)]
    fn decl(&mut self, kind: DeclKind, names: &[Token]) {}

    /// Identifier used in an expression (not after `.`; not def/param names).
    #[inline(always)]
    fn name(&mut self, tok: Token) {}
    /// Attribute name after `.`.
    #[inline(always)]
    fn attribute(&mut self, tok: Token) {}
    /// Plain (non-f/t) string literal.
    #[inline(always)]
    fn string(&mut self, tok: Token) {}

    #[inline(always)]
    fn diagnostic(&mut self, diag: &Diagnostic) {}
}

/// Visitor that ignores everything.
pub struct NoopVisitor;

impl Visitor for NoopVisitor {}
