//! Metrics tests with hand-computed values (comments show the arithmetic).

use super::metrics::{DefMetrics, MetricsScratch, MetricsTable, MetricsVisitor, compute, halstead_volume, maintainability_index};
use super::{Tee, Visitor, parse};
use crate::extract::events::Def;

fn rows(src: &str) -> Vec<DefMetrics> {
    let t = compute(src.as_bytes());
    (0..t.len()).map(|i| t.row(i)).collect()
}

fn one(src: &str) -> DefMetrics {
    let r = rows(src);
    assert_eq!(r.len(), 1, "expected one definition in {src:?}");
    r[0]
}

/// (cyclomatic, cognitive, max_nesting) of every row.
fn ccn(src: &str) -> Vec<(u32, u32, u32)> {
    rows(src).iter().map(|m| (m.cyclomatic, m.cognitive, u32::from(m.max_nesting))).collect()
}

#[test]
fn trivial_function() {
    let m = one("def f():\n    return 1\n");
    assert_eq!((m.cyclomatic, m.cognitive, m.max_nesting, m.loc, m.params), (1, 0, 0, 2, 0));
}

#[test]
fn if_elif_else() {
    let src = "\
def f(x):
    if x > 0:
        return 1
    elif x < 0:
        return -1
    else:
        return 0
";
    // cc: 1 + if + elif = 3. cog: if +1 (nesting 0), elif +1, else +1.
    assert_eq!(ccn(src), vec![(3, 3, 1)]);
}

#[test]
fn nested_ifs_weight_by_nesting() {
    let src = "\
def f(a, b, c):
    if a:
        if b:
            if c:
                pass
";
    // cc: 1 + 3. cog: 1 + 2 + 3.
    assert_eq!(ccn(src), vec![(4, 6, 3)]);
}

#[test]
fn loops_and_loop_else() {
    let src = "\
def f(xs):
    for x in xs:
        while x:
            x -= 1
        else:
            pass
    else:
        pass
";
    // cc: 1 + for + while (loop else not counted) = 3.
    // cog: for +1, while +2 (nesting 1), while-else +1, for-else +1 = 5.
    assert_eq!(ccn(src), vec![(3, 5, 2)]);
}

#[test]
fn async_for_and_with() {
    let src = "\
async def f(xs):
    async with lock:
        async for x in xs:
            pass
";
    // with adds nothing to cc/cog but is a nesting level for max_nesting.
    assert_eq!(ccn(src), vec![(2, 1, 2)]);
}

#[test]
fn try_except_else_finally() {
    let src = "\
def f():
    try:
        a()
    except ValueError:
        b()
    except (KeyError, OSError):
        c()
    else:
        d()
    finally:
        e()
";
    // cc: 1 + 2 handlers + try-else = 4. cog: 2 handlers +1 each, else +1 = 3.
    assert_eq!(ccn(src), vec![(4, 3, 1)]);
}

#[test]
fn try_and_finally_do_not_nest() {
    let src = "\
def f():
    try:
        if a:
            pass
    except E:
        if b:
            pass
    finally:
        if c:
            pass
";
    // cc: 1 + if + except + if + if = 5.
    // cog: if in try +1, except +1, if in except +2, if in finally +1 = 5.
    assert_eq!(ccn(src), vec![(5, 5, 2)]);
}

#[test]
fn bare_except_and_except_star() {
    let src = "\
def f():
    try:
        pass
    except* ValueError:
        pass
    except:
        pass
";
    assert_eq!(ccn(src), vec![(3, 2, 1)]);
}

#[test]
fn with_try_nesting_depth() {
    let src = "\
def f():
    with a:
        try:
            if x:
                pass
        finally:
            pass
";
    // max_nesting counts with/try/if bodies; cognitive nesting does not count with/try.
    assert_eq!(ccn(src), vec![(2, 1, 3)]);
}

#[test]
fn boolean_operator_runs() {
    // Runs: `a and b and c`, `or`, `and` -> +3; cc ignores boolean operators.
    assert_eq!(ccn("def f(a, b, c, d):\n    return a and b and c or d and not a\n"), vec![(1, 3, 0)]);
    // One run of `or`.
    assert_eq!(ccn("def f(a, b, c):\n    return a or b or c\n"), vec![(1, 1, 0)]);
    // Parentheses start a new run.
    assert_eq!(ccn("def f(a, b, c):\n    return a and (b and c)\n"), vec![(1, 2, 0)]);
    // In a condition: if +1, run +1.
    assert_eq!(ccn("def f(a, b):\n    if a and b:\n        pass\n"), vec![(2, 2, 1)]);
    // Separate statements are separate runs.
    assert_eq!(ccn("def f(a, b):\n    x = a and b\n    y = a and b\n"), vec![(1, 2, 0)]);
}

#[test]
fn ternary_and_comprehensions() {
    // Ternary: cog +1 + nesting, not in cc.
    assert_eq!(ccn("def f(c):\n    return 1 if c else 2\n"), vec![(1, 1, 0)]);
    assert_eq!(ccn("def f(c):\n    if c:\n        return 1 if c else 2\n"), vec![(2, 3, 1)]);
    // Comprehension for/if count nowhere.
    assert_eq!(ccn("def f(y):\n    return [x for x in y if x]\n"), vec![(1, 0, 0)]);
    // assert / with / lambda alone count nowhere in cc.
    assert_eq!(ccn("def f(y):\n    assert y\n    with y:\n        g = lambda: y\n"), vec![(1, 0, 1)]);
}

#[test]
fn lambda_increases_nesting() {
    let src = "def f(xs):\n    return sorted(xs, key=lambda x: x if x else 0)\n";
    // Ternary inside a lambda: +1 + nesting 1.
    assert_eq!(ccn(src), vec![(1, 2, 0)]);
    // Nesting is restored after the lambda.
    let src = "def f(xs):\n    g = lambda x: x\n    return 1 if xs else 0\n";
    assert_eq!(ccn(src), vec![(1, 1, 0)]);
}

#[test]
fn match_irrefutable_last_case() {
    let src = "\
def f(x):
    match x:
        case 1:
            return 1
        case [a, b]:
            return 2
        case _:
            return 0
";
    // cc: 1 + 3 cases - 1 (last is `_`, unguarded) = 3. cog: match +1.
    // max_nesting: match/case count once.
    assert_eq!(ccn(src), vec![(3, 1, 1)]);
    // Capture name, parenthesised, alternatives and `as`: irrefutable.
    for last in ["y", "(y)", "1 | _", "_ as z"] {
        let src = format!("def f(x):\n    match x:\n        case 1:\n            pass\n        case {last}:\n            pass\n");
        assert_eq!(ccn(&src), vec![(2, 1, 1)], "{last}");
    }
}

#[test]
fn match_refutable_or_guarded_last_case() {
    // Guarded wildcard: no discount. cc = 1 + 2.
    let src = "def f(x):\n    match x:\n        case 1:\n            pass\n        case _ if x:\n            pass\n";
    assert_eq!(ccn(src), vec![(3, 1, 1)]);
    // Refutable last case.
    let src = "def f(x):\n    match x:\n        case 1:\n            pass\n        case Point(x=0):\n            pass\n";
    assert_eq!(ccn(src), vec![(3, 1, 1)]);
    // An irrefutable case that is not last gets no discount (ruff: last case only).
    let src = "def f(x):\n    match x:\n        case y:\n            pass\n        case 2:\n            pass\n";
    assert_eq!(ccn(src), vec![(3, 1, 1)]);
    // A lone irrefutable case: 1 + 1 - 1.
    let src = "def f(x):\n    match x:\n        case _:\n            pass\n";
    assert_eq!(ccn(src), vec![(1, 1, 1)]);
}

#[test]
fn match_body_is_nested() {
    let src = "\
def f(x):
    match x:
        case 1:
            if x:
                pass
        case _:
            pass
";
    // cc: 1 + 2 cases - 1 + if = 3. cog: match +1, if +2 (nesting 1).
    assert_eq!(ccn(src), vec![(3, 3, 2)]);
}

#[test]
fn nested_function_counts_into_parent() {
    let src = "\
def outer(a):
    if a:
        pass
    def inner(b):
        if b:
            return 1
        return 2
    return inner
";
    // inner: cc 2, cog 1. outer: cc 1 + if + inner's full 2 = 4;
    // cog: if +1, inner's if at nesting 1 (function body) +2 = 3.
    assert_eq!(ccn(src), vec![(4, 3, 1), (2, 1, 1)]);
}

#[test]
fn nested_function_inside_block() {
    let src = "\
def outer(y):
    for x in y:
        def inner():
            if x:
                pass
";
    // inner: cc 2, cog 1, nesting 1. outer: cc 1 + for + 2 = 4;
    // cog: for +1, inner's if at nesting 2 (for body + function) +3 = 4;
    // max_nesting: for (1) + inner's 1 = 2.
    assert_eq!(ccn(src), vec![(4, 4, 2), (2, 1, 1)]);
}

#[test]
fn doubly_nested_functions() {
    let src = "\
def a():
    def b():
        def c():
            if x:
                pass
";
    // c: cc 2, cog 1. b: cc 1 + 2 = 3, cog: if at nesting 1 -> 2.
    // a: cc 1 + 3 = 4, cog: if at nesting 2 -> 3.
    assert_eq!(ccn(src), vec![(4, 3, 1), (3, 2, 1), (2, 1, 1)]);
}

#[test]
fn class_is_transparent() {
    let src = "\
def factory():
    class C:
        def m(self):
            if self:
                pass
    return C
";
    // m: cc 2, cog 1. C (class row, plain sums): cc 2, cog 1.
    // factory: cc 1 + (method +1, if +1) = 3 as ruff (ClassDef body is
    // inlined); cog: if inside a method of a class inside a function: nesting 1 -> 2.
    assert_eq!(ccn(src), vec![(3, 2, 1), (2, 1, 1), (2, 1, 1)]);
}

#[test]
fn classes_inside_functions_shift_correctly() {
    // Class-level statement: classes add no nesting -> if +1.
    assert_eq!(ccn("def f():\n    class C:\n        if x:\n            pass\n"), vec![(2, 1, 1), (1, 1, 1)]);
    // Method of a class inside a `for` of a function: for +1, if at nesting
    // 2 (for body + method body) +3.
    let src = "\
def f(y):
    for x in y:
        class C:
            def m(self):
                if x:
                    pass
";
    assert_eq!(ccn(src), vec![(4, 4, 2), (2, 1, 1), (2, 1, 1)]);
    // Class in a class in a function: if in the method at nesting 1 -> 2.
    let src = "\
def f():
    class A:
        class B:
            def m(self):
                if x:
                    pass
";
    assert_eq!(ccn(src), vec![(3, 2, 1), (2, 1, 1), (2, 1, 1), (2, 1, 1)]);
}

#[test]
fn methods_of_top_level_class_are_independent() {
    let src = "\
class C:
    def a(self):
        if self:
            pass
    def b(self):
        for x in self:
            if x:
                pass
";
    // a: cc 2, cog 1. b: cc 3, cog 1 + 2 = 3. C: sums cc 5, cog 4.
    assert_eq!(ccn(src), vec![(5, 4, 2), (2, 1, 1), (3, 3, 2)]);
}

#[test]
fn direct_recursion() {
    let src = "\
def fact(n):
    if n <= 1:
        return 1
    return n * fact(n - 1)
";
    // cog: if +1, recursion +1.
    assert_eq!(ccn(src), vec![(2, 2, 1)]);
    // Counted once per function, not per call.
    assert_eq!(ccn("def fib(n):\n    return fib(n - 1) + fib(n - 2)\n"), vec![(1, 1, 0)]);
    // Methods through self / cls (the class row sums them).
    let src = "class T:\n    def walk(self, n):\n        for c in n:\n            self.walk(c)\n";
    assert_eq!(ccn(src), vec![(2, 2, 1), (2, 2, 1)]);
    let src = "class T:\n    @classmethod\n    def make(cls, n):\n        return cls.make(n)\n";
    assert_eq!(ccn(src), vec![(1, 1, 0), (1, 1, 0)]);
    // Not recursion: another receiver, self.x in a plain function, a different name.
    let src = "def walk(self, n):\n    other.walk(n)\n    self.walk(n)\n    walker(n)\n";
    assert_eq!(ccn(src), vec![(1, 0, 0)]);
    // Only the innermost function is checked: a nested function calling its parent is not counted.
    let src = "def outer():\n    def inner():\n        outer()\n";
    assert_eq!(ccn(src), vec![(2, 0, 0), (1, 0, 0)]);
    // Recursion of a nested function counts towards the parent (flat +1).
    let src = "def outer():\n    def inner(n):\n        return inner(n)\n";
    assert_eq!(ccn(src), vec![(2, 1, 0), (1, 1, 0)]);
    // Recursion inside a lambda of the function.
    assert_eq!(ccn("def f(n):\n    g = lambda: f(n)\n"), vec![(1, 1, 0)]);
}

#[test]
fn header_events_belong_to_enclosing_scope() {
    // The ternary in the default value and the boolean run in the decorator
    // are evaluated in the enclosing function.
    let src = "\
def outer(c):
    @deco(a and b)
    def inner(x=1 if c else 2):
        pass
";
    // outer: cc 1 + inner 1, cog: run +1, ternary +1 (nesting 0); inner: 0.
    assert_eq!(ccn(src), vec![(2, 2, 0), (1, 0, 0)]);
    // A recursive-looking call in a default is the enclosing scope's.
    assert_eq!(ccn("def f(x=f()):\n    pass\n"), vec![(1, 0, 0)]);
}

#[test]
fn loc_and_params() {
    let src = "\
@dec
def f(a, /, b, *args, c, **kw):
    x = 1

    return x
# trailing comment
";
    let m = one(src);
    // Inclusive from the decorator to the last token.
    assert_eq!(m.loc, 5);
    assert_eq!(m.params, 5);
    let r = rows("class C(Base):\n    x = 1\n    def m(self):\n        pass\n");
    assert_eq!((r[0].loc, r[0].params), (4, 0));
    assert_eq!((r[1].loc, r[1].params), (2, 1));
    // One-line definition.
    assert_eq!(one("def f(): return 1\n").loc, 1);
    // Multi-line last token (triple-quoted string).
    assert_eq!(one("def f():\n    return '''a\nb\nc'''\n").loc, 4);
}

/// (n1, n2, N1, N2) of every row.
fn hal(src: &str) -> Vec<(u32, u32, u32, u32)> {
    rows(src).iter().map(|m| (m.distinct_operators, m.distinct_operands, m.operators, m.operands)).collect()
}

#[test]
fn halstead_simple() {
    let m = one("def f(a, b):\n    return a + b\n");
    // Operators: def ( , : return +  (`)` ignored) -> n1 6, N1 6.
    // Operands: f a b a b -> n2 3, N2 5.
    assert_eq!((m.distinct_operators, m.distinct_operands, m.operators, m.operands), (6, 3, 6, 5));
    // V = 11 log2 9.
    let v = 11.0 * 9f64.log2();
    assert!((f64::from(m.volume) - v).abs() < 1e-3, "{}", m.volume);
    assert!((v - 34.869).abs() < 1e-3);
    // MI = (171 - 5.2 ln V - 0.23 - 16.2 ln 2) * 100 / 171 = 82.50.
    let mi = (171.0 - 5.2 * v.ln() - 0.23 * 1.0 - 16.2 * 2f64.ln()) * 100.0 / 171.0;
    assert!((f64::from(m.mi) - mi).abs() < 1e-3, "{}", m.mi);
    assert!((mi - 82.50).abs() < 0.01, "{mi}");
}

#[test]
fn halstead_token_roles() {
    // Strings, numbers and constants are operands; brackets count once per
    // pair; f-string start is an operator, its literal text an operand,
    // its end ignored.
    let m = one("def f():\n    return [1, 'x', None, f'a{b}']\n");
    // Operators: def ( : return [ , , , f' { -> distinct def ( : return [ , f' { = 8, N1 10.
    // Operands: f 1 'x' None a b -> 6 distinct, 6 total.
    assert_eq!((m.distinct_operators, m.distinct_operands, m.operators, m.operands), (8, 6, 10, 6));
    // Same text, different quoting: different operands.
    assert_eq!(hal("def f():\n    return 'a' + \"a\" + 'a'\n"), vec![(5, 3, 6, 4)]);
}

#[test]
fn role_table_matches_reference() {
    use super::metrics::{token_role, token_role_slow};
    use crate::lex::TokenKind;
    // Every discriminant up to the last error kind.
    let mut n = 0;
    for b in 0..=u8::MAX {
        // Kinds are contiguous from 0: stop at the first byte that is not a kind.
        let Some(k) = kind_from_u8(b) else { break };
        assert_eq!(token_role(k), token_role_slow(k), "{k:?}");
        n += 1;
    }
    assert_eq!(n, TokenKind::ErrorTooDeep as usize + 1);
}

/// `TokenKind` with discriminant `b`, found by lexing nothing: built from the
/// known contiguous range without unsafe.
fn kind_from_u8(b: u8) -> Option<crate::lex::TokenKind> {
    use crate::lex::TokenKind as K;
    const ALL: &[K] = &[
        K::EndOfFile, K::Newline, K::NonLogicalNewline, K::Indent, K::Dedent, K::Comment, K::Name, K::Int, K::Float,
        K::Imaginary, K::String, K::FStringStart, K::FStringMiddle, K::FStringEnd, K::TStringStart, K::TStringMiddle,
        K::TStringEnd, K::False, K::None, K::True, K::And, K::As, K::Assert, K::Async, K::Await, K::Break, K::Class,
        K::Continue, K::Def, K::Del, K::Elif, K::Else, K::Except, K::Finally, K::For, K::From, K::Global, K::If,
        K::Import, K::In, K::Is, K::Lambda, K::Nonlocal, K::Not, K::Or, K::Pass, K::Raise, K::Return, K::Try,
        K::While, K::With, K::Yield, K::LPar, K::RPar, K::LSqb, K::RSqb, K::LBrace, K::RBrace, K::Colon, K::Comma,
        K::Semi, K::Plus, K::Minus, K::Star, K::Slash, K::VBar, K::Amper, K::Less, K::Greater, K::Equal, K::Dot,
        K::Percent, K::EqEqual, K::NotEqual, K::LessEqual, K::GreaterEqual, K::Tilde, K::Circumflex, K::LeftShift,
        K::RightShift, K::DoubleStar, K::PlusEqual, K::MinusEqual, K::StarEqual, K::SlashEqual, K::PercentEqual,
        K::AmperEqual, K::VBarEqual, K::CircumflexEqual, K::LeftShiftEqual, K::RightShiftEqual, K::DoubleStarEqual,
        K::DoubleSlash, K::DoubleSlashEqual, K::At, K::AtEqual, K::RArrow, K::Ellipsis, K::ColonEqual,
        K::Exclamation, K::ErrorUnknownChar, K::ErrorUnterminatedString, K::ErrorUnterminatedTripleString,
        K::ErrorUnterminatedFString, K::ErrorFString, K::ErrorInvalidNumber, K::ErrorBadContinuation, K::ErrorDedent,
        K::ErrorTabs, K::ErrorTooDeep,
    ];
    let k = *ALL.get(b as usize)?;
    assert_eq!(k as u8, b, "kind list out of order at {k:?}");
    Some(k)
}

#[test]
fn halstead_malformed_header_operators_go_to_parent() {
    // `def` without a name: no definition; its tokens are the parent's.
    // f: operators def ( : def - + -> 5 distinct (`-` only in the broken header), 6 total;
    // operands f x 1 -> 3, 3.
    assert_eq!(hal("def f():\n    def -\n    x + 1\n"), vec![(5, 3, 6, 3)]);
}

#[test]
fn halstead_excludes_decorators_and_module_tokens() {
    // `@dec` belongs to the module: operators def ( : pass, operands f.
    assert_eq!(hal("@dec\ndef f(): pass\n"), vec![(4, 1, 4, 1)]);
    // A module-level name used again inside is still new for the function.
    assert_eq!(hal("x = 1\ndef f():\n    return x\n"), vec![(4, 2, 4, 2)]);
    // Siblings are independent.
    assert_eq!(hal("def f(a):\n    return a\ndef g(a):\n    return a\n"), vec![(4, 2, 4, 3), (4, 2, 4, 3)]);
}

#[test]
fn halstead_nested_is_inclusive() {
    let src = "\
def f(a):
    def g(a):
        return a
    return g
";
    // g: operators def ( : return -> (4, 4); operands g a a -> (2, 3).
    // f: operators def ( : def ( : return return -> n1 4, N1 8;
    //    operands f a g a a g -> n2 3, N2 6.
    assert_eq!(hal(src), vec![(4, 3, 8, 6), (4, 2, 4, 3)]);
    // A value first seen in the child is new for both; seen in the parent
    // before the child, new only for the child.
    let src = "def f(a):\n    a\n    def g():\n        a + z\n";
    // g: operators def ( : + -> 4; operands g a z -> 3 distinct.
    // f: operators def ( : def ( : + -> 4 distinct, 7 total;
    //    operands f a a g a z -> 4 distinct (f a g z), 6 total.
    assert_eq!(hal(src), vec![(4, 4, 7, 6), (4, 3, 4, 3)]);
}

#[test]
fn halstead_volume_and_mi_edge_cases() {
    assert_eq!(halstead_volume(0, 0, 0, 0), 0.0);
    assert_eq!(halstead_volume(1, 0, 1, 0), 0.0);
    // V < 1 and LOC 1: MI = (171 - 0.23 CC) * 100 / 171.
    assert!((maintainability_index(0.0, 1, 1) - (170.77 * 100.0 / 171.0)).abs() < 1e-9);
    // Clamped at 0 and 100.
    assert_eq!(maintainability_index(1e30, 1000, 1_000_000), 0.0);
    assert!(maintainability_index(0.0, 0, 0) <= 100.0);
}

#[test]
fn crlf_and_cr_line_ends() {
    for nl in ["\n", "\r\n", "\r"] {
        let src = format!("def f(x):{nl}    if x:{nl}        return 1{nl}{nl}    return 2{nl}");
        let m = one(&src);
        assert_eq!((m.cyclomatic, m.cognitive, m.loc), (2, 1, 5), "{nl:?}");
    }
}

/// Counts definitions (stands in for the facts visitor).
#[derive(Default)]
struct CountDefs {
    defs: usize,
    classes: Vec<bool>,
}

impl Visitor for CountDefs {
    fn def_enter(&mut self, d: &Def<'_>) {
        self.defs += 1;
        self.classes.push(d.kind == crate::extract::events::DefKind::Class);
    }
}

#[test]
fn tee_and_scratch_reuse() {
    let a = "def f(x):\n    if x:\n        pass\nclass C:\n    def m(self): return 1 if self else 2\n";
    let b = "def g(a, b):\n    return a and b\n";
    let mut scratch = MetricsScratch::new();
    let mut table = MetricsTable::new();
    let counts = parse(a.as_bytes(), Tee(CountDefs::default(), MetricsVisitor::new(a.as_bytes(), &mut scratch, &mut table))).0;
    assert_eq!(counts.defs, 3);
    assert_eq!(table.len(), 3);
    // Reused scratch, rows appended to the same table.
    parse(b.as_bytes(), MetricsVisitor::new(b.as_bytes(), &mut scratch, &mut table));
    let mut expected = compute(a.as_bytes());
    expected.append(&compute(b.as_bytes()));
    assert_eq!(table, expected);
    assert_eq!(table.row(3).cognitive, 1);
    assert!(table.heap_bytes() >= 40 * table.len());
    assert!(scratch.heap_bytes() > 0);
    table.clear();
    assert!(table.is_empty());
}

/// Checks invariants of the metrics of `src` (any bytes).
fn check_invariants(src: &[u8], what: &str) {
    let t = compute(src);
    let counts = parse(src, CountDefs::default());
    assert_eq!(t.len(), counts.defs, "{what}");
    for i in 0..t.len() {
        let m = t.row(i);
        if !counts.classes[i] {
            assert!(m.cyclomatic >= 1, "{what} row {i}");
        }
        assert!(m.loc >= 1, "{what} row {i}");
        assert!(m.distinct_operators <= m.operators && m.distinct_operands <= m.operands, "{what} row {i}");
        assert!(m.volume.is_finite() && m.volume >= 0.0, "{what} row {i}");
        assert!((0.0..=100.0).contains(&m.mi), "{what} row {i}");
    }
}

fn corpus_files() -> Vec<(std::path::PathBuf, Vec<u8>)> {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/corpus");
    let mut stack = vec![root];
    let mut out = Vec::new();
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else if p.extension().is_some_and(|x| x == "py" || x == "pyi")
                && let Ok(src) = std::fs::read(&p)
            {
                out.push((p, src));
            }
        }
    }
    out.sort();
    out
}

#[test]
fn corpus_invariants() {
    let files = corpus_files();
    assert!(!files.is_empty(), "tests/corpus not found");
    for (p, src) in &files {
        check_invariants(src, &p.display().to_string());
    }
}

#[test]
fn deep_lambda_nesting_is_linear_and_saturates() {
    // Nested lambdas whose bodies are ternaries: the score is quadratic in the
    // nesting (arithmetic saturates instead of overflowing). Small case
    // exactly, then a big one without overflow (debug builds check).
    let src = "def f():\n    return lambda: lambda: (1 if a else 2)\n";
    // Ternary at nesting 2: +3.
    assert_eq!(ccn(src), vec![(1, 3, 0)]);
    let n = 20_000;
    let mut big = String::from("def f():\n    return ");
    for _ in 0..n {
        big.push_str("lambda: (1 if a else ");
    }
    big.push('2');
    for _ in 0..n {
        big.push(')');
    }
    big.push('\n');
    let m = one(&big);
    // Ternary k (0-based) sits at nesting k + 1: sum = n(n+3)/2 (fits in u32 here).
    assert_eq!(u64::from(m.cognitive), (n as u64) * (n as u64 + 3) / 2);
    // Nested functions multiply nothing quadratically beyond that: a chain of
    // nested defs with one ternary each stays exact.
    let mut chain = String::new();
    for d in 0..50 {
        let ind = "    ".repeat(d);
        chain.push_str(&format!("{ind}def f{d}():\n{ind}    x = 1 if a else 2\n"));
    }
    let r = rows(&chain);
    // f0: ternary at nesting k in f_k counts 1 + k towards f0: sum_{k<50} (1 + k) = 1275.
    assert_eq!(r[0].cognitive, 1275);
    assert_eq!(r[49].cognitive, 1);
}

#[test]
fn malformed_inputs_do_not_panic() {
    // Truncations of every corpus file (unbalanced brackets, cut headers,
    // missing bodies) plus hand-written broken definitions.
    for (p, src) in corpus_files() {
        let step = (src.len() / 24).max(1);
        let mut cut = 0;
        while cut < src.len() {
            check_invariants(&src[..cut], &format!("{} cut {cut}", p.display()));
            cut += step;
        }
    }
    for src in [
        "def\n    if x:\n        pass\n",
        "def (x):\n    pass\n",
        "class\nif a:\n    pass\n",
        "def f(:\n    if x:\n        pass\n",
        "def f(x=(:\n    pass\ndef g(): pass\n",
        "@dec\n\nx = 1\n",
        "def f():\nreturn 1\n",
        "def f(x):\n    match x:\n        case 1:\n    case _:\n",
        "lambda: (lambda: def\n",
        "def f(): def g(): pass\n",
    ] {
        check_invariants(src.as_bytes(), src);
    }
}
