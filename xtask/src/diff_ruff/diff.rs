//! Diff engine: compares the oracle's [`FileFacts`] with a candidate's for one file.
//!
//! Matching is hash-indexed (O(n) per file): the oracle side is indexed by key in a
//! reusable multimap, each candidate fact takes the first unmatched oracle fact with an equal
//! key, and leftovers on either side are reported as missing / extra.
//! - defs are keyed by qualname and matched in source order (so the k-th `A.x` pairs with
//!   the k-th `A.x`); matched pairs are then compared field by field.
//! - imports and calls are multisets keyed by all their fields (scope compared by qualname,
//!   never by index, so the two sides may number their defs differently).

use std::collections::HashMap;
use std::fmt;
use std::hash::Hasher;

use super::facts::{Bytes, Call, Def, FileFacts, Import};
use super::hash::{FxBuild, FxHasher};

/// Mismatch categories, in report order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Category {
    DefMissing,
    DefExtra,
    DefKind,
    DefLine,
    DefDecorators,
    DefBases,
    DefParams,
    DefMccabe,
    ImportMissing,
    ImportExtra,
    CallMissing,
    CallExtra,
    /// The oracle produced facts but the candidate could not.
    CandidateSkip,
}

impl Category {
    pub const COUNT: usize = 13;
    pub const ALL: [Category; Self::COUNT] = [
        Category::DefMissing,
        Category::DefExtra,
        Category::DefKind,
        Category::DefLine,
        Category::DefDecorators,
        Category::DefBases,
        Category::DefParams,
        Category::DefMccabe,
        Category::ImportMissing,
        Category::ImportExtra,
        Category::CallMissing,
        Category::CallExtra,
        Category::CandidateSkip,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Category::DefMissing => "defs.missing",
            Category::DefExtra => "defs.extra",
            Category::DefKind => "defs.kind",
            Category::DefLine => "defs.line",
            Category::DefDecorators => "defs.decorators",
            Category::DefBases => "defs.bases",
            Category::DefParams => "defs.params",
            Category::DefMccabe => "defs.mccabe",
            Category::ImportMissing => "imports.missing",
            Category::ImportExtra => "imports.extra",
            Category::CallMissing => "calls.missing",
            Category::CallExtra => "calls.extra",
            Category::CandidateSkip => "files.candidate-skip",
        }
    }
}

/// Receives mismatches. `msg` is lazy: implementations format it only if they keep it, so
/// counting a mismatch never allocates.
pub trait Sink {
    fn mismatch(&mut self, cat: Category, line: u32, msg: fmt::Arguments<'_>);
}

const NIL: u32 = u32::MAX;

/// Reusable hash multimap from key hash to a chain of oracle indexes (in source order).
/// Matched entries are unlinked so duplicate keys cost O(1) each; collisions are resolved by
/// the caller's equality check.
#[derive(Default)]
struct MultiIndex {
    heads: HashMap<u64, u32, FxBuild>,
    next: Vec<u32>,
}

impl MultiIndex {
    fn build(&mut self, n: usize, mut hash: impl FnMut(usize) -> u64) {
        self.heads.clear();
        self.next.clear();
        self.next.resize(n, NIL);
        // Insert back to front so each chain is in ascending (source) order.
        for i in (0..n).rev() {
            let prev = self.heads.insert(hash(i), i as u32).unwrap_or(NIL);
            self.next[i] = prev;
        }
    }

    /// Removes and returns the first chained index with hash `h` satisfying `eq`.
    fn take(&mut self, h: u64, mut eq: impl FnMut(usize) -> bool) -> Option<usize> {
        let head = self.heads.get_mut(&h)?;
        let mut prev = NIL;
        let mut cur = *head;
        while cur != NIL {
            let i = cur as usize;
            if eq(i) {
                let after = self.next[i];
                if prev == NIL {
                    *head = after;
                } else {
                    self.next[prev as usize] = after;
                }
                return Some(i);
            }
            prev = cur;
            cur = self.next[i];
        }
        None
    }
}

/// Per-thread scratch for [`diff_file`]; reuse across files so steady state is allocation-free.
#[derive(Default)]
pub struct DiffScratch {
    index: MultiIndex,
    matched: Vec<bool>,
}

fn h_def(f: &FileFacts, d: &Def) -> u64 {
    let mut h = FxHasher::default();
    h.write(f.str(d.qualname));
    h.finish()
}

fn h_import(f: &FileFacts, i: &Import) -> u64 {
    let mut h = FxHasher::default();
    h.write(f.scope_name(i.scope));
    h.write_u8(0);
    h.write(f.str(i.module));
    h.write_u8(0);
    h.write(f.str(i.name));
    h.write_u8(0);
    h.write(f.str(i.alias));
    h.write_u32(i.level);
    h.write_u32(i.line);
    h.finish()
}

fn eq_import(a: &FileFacts, x: &Import, b: &FileFacts, y: &Import) -> bool {
    x.line == y.line
        && x.level == y.level
        && a.str(x.module) == b.str(y.module)
        && a.str(x.name) == b.str(y.name)
        && a.str(x.alias) == b.str(y.alias)
        && a.scope_name(x.scope) == b.scope_name(y.scope)
}

fn h_call(f: &FileFacts, c: &Call) -> u64 {
    let mut h = FxHasher::default();
    h.write(f.scope_name(c.scope));
    h.write_u8(0);
    h.write(f.str(c.callee));
    h.write_u32(c.line);
    h.finish()
}

fn eq_call(a: &FileFacts, x: &Call, b: &FileFacts, y: &Call) -> bool {
    x.line == y.line && a.str(x.callee) == b.str(y.callee) && a.scope_name(x.scope) == b.scope_name(y.scope)
}

/// Outcome of matching one fact.
enum Outcome<'a, T> {
    Pair(&'a T, &'a T),
    /// Candidate fact with no oracle counterpart.
    Extra(&'a T),
    /// Oracle fact with no candidate counterpart.
    Missing(&'a T),
}

/// Generic multiset match. Reports pairs and extras in candidate order, then missing facts in
/// oracle order.
fn match_items<'a, T>(
    scratch: &mut DiffScratch,
    oracle: &'a [T],
    cand: &'a [T],
    hash_o: impl Fn(&T) -> u64,
    hash_c: impl Fn(&T) -> u64,
    eq: impl Fn(&T, &T) -> bool,
    mut on: impl FnMut(Outcome<'a, T>),
) {
    scratch.index.build(oracle.len(), |i| hash_o(&oracle[i]));
    scratch.matched.clear();
    scratch.matched.resize(oracle.len(), false);
    for y in cand {
        match scratch.index.take(hash_c(y), |i| eq(&oracle[i], y)) {
            Some(i) => {
                scratch.matched[i] = true;
                on(Outcome::Pair(&oracle[i], y));
            }
            None => on(Outcome::Extra(y)),
        }
    }
    for (x, &m) in oracle.iter().zip(&scratch.matched) {
        if !m {
            on(Outcome::Missing(x));
        }
    }
}

/// Compares the facts of one file, reporting every mismatch to `sink`.
/// `o` is the oracle (ruff), `c` the candidate.
pub fn diff_file(o: &FileFacts, c: &FileFacts, scratch: &mut DiffScratch, sink: &mut impl Sink) {
    match_items(
        scratch,
        &o.defs,
        &c.defs,
        |x| h_def(o, x),
        |y| h_def(c, y),
        |x, y| o.str(x.qualname) == c.str(y.qualname),
        |out| match out {
            Outcome::Pair(x, y) => compare_def(o, x, c, y, sink),
            Outcome::Extra(y) => sink.mismatch(
                Category::DefExtra,
                y.line,
                format_args!("{} `{}`", y.kind.as_str(), Bytes(c.str(y.qualname))),
            ),
            Outcome::Missing(x) => sink.mismatch(
                Category::DefMissing,
                x.line,
                format_args!("{} `{}`", x.kind.as_str(), Bytes(o.str(x.qualname))),
            ),
        },
    );

    match_items(
        scratch,
        &o.imports,
        &c.imports,
        |x| h_import(o, x),
        |y| h_import(c, y),
        |x, y| eq_import(o, x, c, y),
        |out| match out {
            Outcome::Pair(..) => {}
            Outcome::Extra(y) => sink.mismatch(Category::ImportExtra, y.line, format_args!("{}", ImportFmt(c, y))),
            Outcome::Missing(x) => sink.mismatch(Category::ImportMissing, x.line, format_args!("{}", ImportFmt(o, x))),
        },
    );

    match_items(
        scratch,
        &o.calls,
        &c.calls,
        |x| h_call(o, x),
        |y| h_call(c, y),
        |x, y| eq_call(o, x, c, y),
        |out| match out {
            Outcome::Pair(..) => {}
            Outcome::Extra(y) => sink.mismatch(
                Category::CallExtra,
                y.line,
                format_args!("`{}` in `{}`", Bytes(c.str(y.callee)), Bytes(c.scope_name(y.scope))),
            ),
            Outcome::Missing(x) => sink.mismatch(
                Category::CallMissing,
                x.line,
                format_args!("`{}` in `{}`", Bytes(o.str(x.callee)), Bytes(o.scope_name(x.scope))),
            ),
        },
    );
}

fn compare_def(o: &FileFacts, x: &Def, c: &FileFacts, y: &Def, sink: &mut impl Sink) {
    let name = Bytes(o.str(x.qualname));
    if x.kind != y.kind {
        sink.mismatch(Category::DefKind, x.line, format_args!("`{name}`: ruff {}, ours {}", x.kind.as_str(), y.kind.as_str()));
    }
    if x.line != y.line {
        sink.mismatch(Category::DefLine, x.line, format_args!("`{name}`: ruff {}, ours {}", x.line, y.line));
    }
    if o.str(x.decorators) != c.str(y.decorators) {
        sink.mismatch(
            Category::DefDecorators,
            x.line,
            format_args!("`{name}`: ruff `{}`, ours `{}`", Bytes(o.str(x.decorators)), Bytes(c.str(y.decorators))),
        );
    }
    if o.str(x.bases) != c.str(y.bases) {
        sink.mismatch(
            Category::DefBases,
            x.line,
            format_args!("`{name}`: ruff `{}`, ours `{}`", Bytes(o.str(x.bases)), Bytes(c.str(y.bases))),
        );
    }
    if x.params != y.params {
        sink.mismatch(Category::DefParams, x.line, format_args!("`{name}`: ruff {}, ours {}", x.params, y.params));
    }
    if x.mccabe != y.mccabe {
        sink.mismatch(Category::DefMccabe, x.line, format_args!("`{name}`: ruff {}, ours {}", x.mccabe, y.mccabe));
    }
}

struct ImportFmt<'a>(&'a FileFacts, &'a Import);

impl fmt::Display for ImportFmt<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (ff, i) = (self.0, self.1);
        let name = ff.str(i.name);
        if name.is_empty() {
            write!(f, "import {}", Bytes(ff.str(i.module)))?;
        } else {
            f.write_str("from ")?;
            for _ in 0..i.level {
                f.write_str(".")?;
            }
            write!(f, "{} import {}", Bytes(ff.str(i.module)), Bytes(name))?;
        }
        let alias = ff.str(i.alias);
        if !alias.is_empty() {
            write!(f, " as {}", Bytes(alias))?;
        }
        write!(f, " in `{}`", Bytes(ff.scope_name(i.scope)))
    }
}

#[cfg(test)]
mod tests {
    use super::super::facts::{DefKind, MODULE_SCOPE, Str};
    use super::*;

    /// Collects every mismatch as "category line: message".
    #[derive(Default)]
    struct Collect(Vec<String>);

    impl Sink for Collect {
        fn mismatch(&mut self, cat: Category, line: u32, msg: fmt::Arguments<'_>) {
            self.0.push(format!("{} {line}: {msg}", cat.as_str()));
        }
    }

    /// Terse FileFacts builder for tests.
    #[derive(Default)]
    struct B(FileFacts);

    impl B {
        fn func(self, parent: u32, name: &str, line: u32, params: u32, mccabe: u32) -> Self {
            self.def(DefKind::Function, parent, name, line, "", "", params, mccabe)
        }

        #[allow(clippy::too_many_arguments)]
        fn def(mut self, kind: DefKind, parent: u32, qualname: &str, line: u32, decos: &str, bases: &str, params: u32, mccabe: u32) -> Self {
            let f = &mut self.0;
            let qualname = f.push_str(qualname.as_bytes());
            let decorators = f.push_str(decos.as_bytes());
            let bases = f.push_str(bases.as_bytes());
            f.defs.push(Def { kind, parent, qualname, line, decorators, bases, params, mccabe });
            self
        }

        fn import(mut self, scope: u32, module: &str, name: &str, alias: &str, level: u32, line: u32) -> Self {
            let f = &mut self.0;
            let module = f.push_str(module.as_bytes());
            let name = f.push_str(name.as_bytes());
            let alias = if alias.is_empty() { Str::default() } else { f.push_str(alias.as_bytes()) };
            f.imports.push(Import { scope, module, name, alias, level, line });
            self
        }

        fn call(mut self, scope: u32, callee: &str, line: u32) -> Self {
            let callee = self.0.push_str(callee.as_bytes());
            self.0.calls.push(Call { scope, callee, line });
            self
        }
    }

    fn diff(o: &B, c: &B) -> Vec<String> {
        let mut sink = Collect::default();
        let mut scratch = DiffScratch::default();
        diff_file(&o.0, &c.0, &mut scratch, &mut sink);
        sink.0
    }

    fn sample() -> B {
        B::default()
            .def(DefKind::Class, MODULE_SCOPE, "A", 3, "@dataclass", "Base,metaclass=M", 0, 0)
            .func(0, "A.f", 4, 2, 3)
            .import(MODULE_SCOPE, "os", "", "", 0, 1)
            .import(1, "pkg", "x", "y", 1, 5)
            .call(1, "self.g", 6)
            .call(MODULE_SCOPE, "print", 9)
    }

    #[test]
    fn identical_facts_have_no_mismatches() {
        assert!(diff(&sample(), &sample()).is_empty());
        assert!(diff(&B::default(), &B::default()).is_empty());
    }

    #[test]
    fn missing_and_extra_everywhere() {
        let o = sample();
        let c = B::default()
            .def(DefKind::Class, MODULE_SCOPE, "A", 3, "@dataclass", "Base,metaclass=M", 0, 0)
            .func(0, "A.h", 7, 1, 1)
            .import(MODULE_SCOPE, "sys", "", "", 0, 1)
            .call(MODULE_SCOPE, "print", 9)
            .call(MODULE_SCOPE, "print", 10);
        assert_eq!(
            diff(&o, &c),
            [
                "defs.extra 7: function `A.h`",
                "defs.missing 4: function `A.f`",
                "imports.extra 1: import sys in `<module>`",
                "imports.missing 1: import os in `<module>`",
                "imports.missing 5: from .pkg import x as y in `A.f`",
                "calls.extra 10: `print` in `<module>`",
                "calls.missing 6: `self.g` in `A.f`",
            ]
        );
    }

    #[test]
    fn def_field_mismatches_each_get_a_category() {
        let o = sample();
        let c = B::default()
            .def(DefKind::Function, MODULE_SCOPE, "A", 2, "@dataclass(frozen=True)", "Base", 1, 1)
            .func(0, "A.f", 4, 3, 4)
            .import(MODULE_SCOPE, "os", "", "", 0, 1)
            .import(1, "pkg", "x", "y", 1, 5)
            .call(1, "self.g", 6)
            .call(MODULE_SCOPE, "print", 9);
        assert_eq!(
            diff(&o, &c),
            [
                "defs.kind 3: `A`: ruff class, ours function",
                "defs.line 3: `A`: ruff 3, ours 2",
                "defs.decorators 3: `A`: ruff `@dataclass`, ours `@dataclass(frozen=True)`",
                "defs.bases 3: `A`: ruff `Base,metaclass=M`, ours `Base`",
                "defs.params 3: `A`: ruff 0, ours 1",
                "defs.mccabe 3: `A`: ruff 0, ours 1",
                "defs.params 4: `A.f`: ruff 2, ours 3",
                "defs.mccabe 4: `A.f`: ruff 3, ours 4",
            ]
        );
    }

    #[test]
    fn duplicate_qualnames_pair_in_source_order() {
        let o = B::default().func(MODULE_SCOPE, "x", 6, 1, 1).func(MODULE_SCOPE, "x", 8, 2, 1);
        assert!(diff(&o, &o).is_empty());
        // Candidate saw only the second definition: it pairs with the first oracle `x`.
        let c = B::default().func(MODULE_SCOPE, "x", 8, 2, 1);
        assert_eq!(
            diff(&o, &c),
            ["defs.line 6: `x`: ruff 6, ours 8", "defs.params 6: `x`: ruff 1, ours 2", "defs.missing 8: function `x`"]
        );
    }

    #[test]
    fn duplicate_calls_are_a_multiset() {
        let o = B::default().call(MODULE_SCOPE, "f", 3).call(MODULE_SCOPE, "f", 3).call(MODULE_SCOPE, "f", 3);
        let c = B::default().call(MODULE_SCOPE, "f", 3);
        assert_eq!(diff(&o, &c), ["calls.missing 3: `f` in `<module>`", "calls.missing 3: `f` in `<module>`"]);
        assert_eq!(diff(&c, &o), ["calls.extra 3: `f` in `<module>`", "calls.extra 3: `f` in `<module>`"]);
    }

    #[test]
    fn scopes_compare_by_qualname_not_index() {
        // Same scopes, different def numbering on each side.
        let o = B::default().func(MODULE_SCOPE, "a", 1, 0, 1).func(MODULE_SCOPE, "b", 2, 0, 1).call(1, "g", 3).import(0, "m", "", "", 0, 1);
        let c = B::default().func(MODULE_SCOPE, "b", 2, 0, 1).func(MODULE_SCOPE, "a", 1, 0, 1).call(0, "g", 3).import(1, "m", "", "", 0, 1);
        assert!(diff(&o, &c).is_empty());
        // Same callee and line but a different scope is a mismatch.
        let c2 = B::default().func(MODULE_SCOPE, "a", 1, 0, 1).func(MODULE_SCOPE, "b", 2, 0, 1).call(0, "g", 3).import(0, "m", "", "", 0, 1);
        assert_eq!(diff(&o, &c2), ["calls.extra 3: `g` in `a`", "calls.missing 3: `g` in `b`"]);
    }

    #[test]
    fn import_fields_all_participate() {
        let base = || B::default().import(MODULE_SCOPE, "m", "n", "a", 1, 2);
        for c in [
            B::default().import(MODULE_SCOPE, "m2", "n", "a", 1, 2),
            B::default().import(MODULE_SCOPE, "m", "n2", "a", 1, 2),
            B::default().import(MODULE_SCOPE, "m", "n", "", 1, 2),
            B::default().import(MODULE_SCOPE, "m", "n", "a", 0, 2),
            B::default().import(MODULE_SCOPE, "m", "n", "a", 1, 3),
        ] {
            let d = diff(&base(), &c);
            assert_eq!(d.len(), 2, "{d:?}");
            assert!(d[0].starts_with("imports.extra") && d[1].starts_with("imports.missing"), "{d:?}");
        }
        assert!(diff(&base(), &base()).is_empty());
    }

    #[test]
    fn multi_index_survives_total_hash_collision() {
        let keys = [5u32, 7, 5, 9, 7, 5];
        let mut idx = MultiIndex::default();
        idx.build(keys.len(), |_| 42);
        // All entries share one chain; equality picks the right ones in source order.
        assert_eq!(idx.take(42, |i| keys[i] == 5), Some(0));
        assert_eq!(idx.take(42, |i| keys[i] == 5), Some(2));
        assert_eq!(idx.take(42, |i| keys[i] == 7), Some(1));
        assert_eq!(idx.take(42, |i| keys[i] == 5), Some(5));
        assert_eq!(idx.take(42, |i| keys[i] == 5), None);
        assert_eq!(idx.take(42, |i| keys[i] == 9), Some(3));
        assert_eq!(idx.take(42, |i| keys[i] == 7), Some(4));
        assert_eq!(idx.take(42, |_| true), None);
        assert_eq!(idx.take(1, |_| true), None);
        // Rebuild reuses the index.
        idx.build(2, |i| i as u64);
        assert_eq!(idx.take(1, |_| true), Some(1));
    }

    #[test]
    fn scratch_reuse_across_files_is_clean() {
        let mut scratch = DiffScratch::default();
        let mut sink = Collect::default();
        let big = (0..1000).fold(B::default(), |b, i| b.call(MODULE_SCOPE, "f", i));
        diff_file(&big.0, &big.0, &mut scratch, &mut sink);
        let small_o = B::default().call(MODULE_SCOPE, "f", 1);
        let small_c = B::default().call(MODULE_SCOPE, "f", 2);
        diff_file(&small_o.0, &small_c.0, &mut scratch, &mut sink);
        assert_eq!(sink.0, ["calls.extra 2: `f` in `<module>`", "calls.missing 1: `f` in `<module>`"]);
    }

    #[test]
    fn large_inputs_are_linear() {
        // 200k calls with one key each; a quadratic matcher would take minutes here.
        let n = 200_000;
        let o = (0..n).fold(B::default(), |b, i| b.call(MODULE_SCOPE, "f", i));
        let c = (0..n).rev().fold(B::default(), |b, i| b.call(MODULE_SCOPE, "f", i));
        assert!(diff(&o, &c).is_empty());
    }
}
