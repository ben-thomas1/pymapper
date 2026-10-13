//! llm format tests on hand-built findings: grouping (chains, referrer cycles, metrics, pairs,
//! import sites), priority, the cap and its trailer, header facts.

use crate::output::tests::TestCtx;
use crate::output::{Options, OutputFormat, Outcome, Report, RunNotes, Summary};
use crate::rules::codes::*;
use crate::rules::dead_code::reason;
use crate::rules::{Arg, FileId, FilterStats, Findings, Location, SymbolId};

fn ctx() -> TestCtx {
    TestCtx {
        paths: vec![b"pkg/a.py".to_vec(), b"pkg/b.py".to_vec(), b"pkg/c.py".to_vec()],
        symbols: vec!["pkg.a:root", "pkg.b:helper", "pkg.b:other", "pkg.a:f", "pkg.c", "pkg.c:g", "pkg.a:Cls.m"],
        names: vec!["json", "os", "yaml", "PyYAML"],
        contracts: vec!["core"],
    }
}

fn at(file: u32, line: u32) -> Location {
    Location::new(FileId(file), line, 5)
}

fn render(f: &Findings, opts: &Options, sum: &Summary<'_>) -> String {
    let c = ctx();
    let r = Report::new(f, &c);
    let mut out = Vec::new();
    assert_eq!(r.write(opts, sum, &mut out).expect("write"), Outcome::Written);
    String::from_utf8(out).expect("utf-8")
}

fn llm(limit: u32) -> Options {
    Options { limit, ..Options::new(OutputFormat::Llm) }
}

/// `root` (never used) <- `helper` (only used by root) <- `other` (only used by helper), plus an
/// unrelated dead `g` and two unused imports in one file.
fn chain() -> Findings {
    let s = SymbolId;
    let mut f = Findings::new();
    let dead = |f: &mut Findings, file, line, sym: u32, phrase, reasons, by: Option<u32>| {
        let extra: Vec<Arg> = by.map(|b| Arg::Symbol(s(b))).into_iter().collect();
        f.push_full(UNUSED_FUNCTION, at(file, line), 0, s(sym), 100, reasons, &[Arg::Symbol(s(sym)), Arg::Phrase(phrase)], &extra);
    };
    dead(&mut f, 0, 3, 0, 0, reason::NO_REFERENCES, None);
    dead(&mut f, 1, 7, 1, 1, reason::ONLY_USED_BY_UNUSED_CODE, Some(0));
    dead(&mut f, 1, 9, 2, 1, reason::ONLY_USED_BY_UNUSED_CODE, Some(1));
    dead(&mut f, 2, 4, 5, 0, reason::NO_REFERENCES, None);
    f.push_with_reasons(UNUSED_IMPORT, at(2, 1), s(4), 100, &[Arg::Name(0)], reason::NO_REFERENCES);
    f.push_with_reasons(UNUSED_IMPORT, at(2, 2), s(4), 100, &[Arg::Name(1)], reason::NO_REFERENCES);
    f
}

#[test]
fn dead_chains_are_one_item() {
    let out = render(&chain(), &llm(0), &Summary::default());
    assert!(out.contains(
        " pkg/a.py:3 root (function): never used (conf 100)\n  also dead if it goes: pkg/b.py: helper:7, other:9\n"
    ), "{out}");
    assert!(out.contains("verify: grep -rnwE --exclude-dir=.venv 'root|helper|other' .\n"), "{out}");
    assert!(out.contains("## Dead code (2 items, 4 symbols)"), "{out}");
    // Unused imports of a file: one item.
    assert!(out.contains(" pkg/c.py: json:1, os:2 (conf 100)\n"), "{out}");
    assert!(out.contains("(likewise for the other) · verify: grep -nwE 'json|os' pkg/c.py\n"), "{out}");
    assert!(out.ends_with(super::NEXT), "{out}");
}

#[test]
fn referrer_cycles_take_their_first_member_as_root() {
    let s = SymbolId;
    let mut f = Findings::new();
    for (sym, by) in [(1u32, 2u32), (2, 1)] {
        let args = [Arg::Symbol(s(sym)), Arg::Phrase(1)];
        f.push_full(UNUSED_FUNCTION, at(1, sym * 10), 0, s(sym), 100, reason::ONLY_USED_BY_UNUSED_CODE, &args, &[Arg::Symbol(s(by))]);
    }
    let out = render(&f, &llm(0), &Summary::default());
    assert!(out.contains(" pkg/b.py:10 helper (function): only used by unused code (conf 100)\n  also dead if it goes: pkg/b.py: other:20\n"), "{out}");
}

#[test]
fn cap_counts_items_and_ends_with_the_trailer() {
    let out = render(&chain(), &llm(1), &Summary::default());
    let lines: Vec<&str> = out.lines().collect();
    let n = lines.len();
    assert_eq!(lines[n - 2], "3 more findings not displayed (in 1 file). Use --limit 0 to show all, or --select <code> to narrow.");
    assert_eq!(format!("{}\n", lines[n - 1]), super::NEXT);
    assert!(out.contains("6 findings in 3 files, 3 items, showing 1"), "{out}");
    assert!(out.contains("## Dead code (2 items, 4 symbols; 1 shown)"), "{out}");
}

#[test]
fn metrics_merge_and_rank_by_overshoot() {
    let s = SymbolId;
    let mut f = Findings::new();
    let m = |v, t| [Arg::Symbol(s(3)), Arg::U32(v), Arg::U32(t)];
    f.push(COMPLEX_FUNCTION, at(0, 5), s(3), 100, &m(11, 10));
    f.push(TOO_MANY_PARAMETERS, at(0, 5), s(3), 100, &m(14, 7));
    let n = [Arg::Symbol(s(6)), Arg::U32(30), Arg::U32(10)];
    f.push(COMPLEX_FUNCTION, at(0, 40), s(6), 100, &n);
    let out = render(&f, &llm(0), &Summary::default());
    // `f` (params 2x the limit) before `Cls.m` (McCabe 3x): worst overshoot first.
    let cls = out.find("Cls.m: McCabe 30>10").expect(&out);
    let fun = out.find("f: McCabe 11>10, params 14>7 (PM101,PM105)").expect(&out);
    assert!(cls < fun, "{out}");
    assert!(out.contains("## Complexity (2 functions)"), "{out}");
    // The fix of the worst metric of the merged item.
    assert!(out.contains("fix: group related parameters of f into a dataclass, or split it, to take at most 7 (now 14)"), "{out}");
    assert!(out.contains("verify: pymapper callers pkg.a:f\n"), "{out}");
}

#[test]
fn pairs_once_and_dependency_sites_merged() {
    let s = SymbolId;
    let mut f = Findings::new();
    f.push_ext(DUPLICATE_CODE, at(0, 10), 20, s(3), 96, &[Arg::U32(11), Arg::File(FileId(1)), Arg::U32(30)], &[
        Arg::File(FileId(1)),
        Arg::U32(30),
        Arg::U32(40),
        Arg::Symbol(s(1)),
    ]);
    f.push_ext(DUPLICATE_CODE, at(1, 30), 40, s(1), 96, &[Arg::U32(11), Arg::File(FileId(0)), Arg::U32(10)], &[
        Arg::File(FileId(0)),
        Arg::U32(10),
        Arg::U32(20),
        Arg::Symbol(s(3)),
    ]);
    f.push(UNDECLARED_DEPENDENCY, at(0, 1), SymbolId::NONE, 90, &[Arg::Name(2), Arg::Name(3)]);
    f.push(UNDECLARED_DEPENDENCY, at(2, 3), SymbolId::NONE, 90, &[Arg::Name(2), Arg::Name(3)]);
    let notes = RunNotes { command: "deps --check", root: "..", ..RunNotes::default() };
    let out = render(&f, &llm(0), &Summary { notes, ..Summary::default() });
    assert_eq!(out.matches("duplicated lines").count(), 1, "{out}");
    assert!(out.contains(" 11 duplicated lines: pkg/a.py:10-20 f = pkg/b.py:30-40 helper (conf 96)\n"), "{out}");
    assert!(out.contains(" pkg/a.py:1 yaml is provided by PyYAML, which is not a declared dependency; also imported at pkg/c.py:3 (conf 90)\n"), "{out}");
    assert!(out.starts_with("# pymapper deps --check · paths relative to the working directory\n"), "{out}");
    assert!(out.contains(super::DEPS_BLIND_SPOTS), "{out}");
    assert!(out.contains("## Dependencies (2 in 1 item)"), "{out}");
}

#[test]
fn header_facts() {
    let modes = ["pkg: library (no [project.scripts] and no plausible __main__ guard)".to_owned(), ".: application (forced by `mode`)".to_owned()];
    let skipped = ["PM501: needs `analysis.dupes = true`".to_owned()];
    let notes = RunNotes { command: "check", root: ".", modes: &modes, skipped: &skipped, filter: "pkg (2 of 3 first-party files)" };
    let filter = FilterStats { below_confidence: 4, suppressed: 2, ..FilterStats::default() };
    let out = render(&chain(), &llm(50), &Summary { files_checked: 3, filter, notes });
    let head: Vec<&str> = out.lines().take(8).collect();
    assert_eq!(head[1], "Dead-code mode per unit:");
    assert_eq!(head[2], "- pkg: library (no [project.scripts] and no plausible __main__ guard)");
    assert_eq!(head[4], "Filtered to pkg (2 of 3 first-party files); the whole project was analysed.");
    assert_eq!(
        head[5],
        "6 findings in 3 files, 3 items, all shown · 4 below confidence 80 not shown (--min-confidence 0 shows them) · 2 suppressed · exit 1 = findings, not an error"
    );
    assert_eq!(head[6], super::CHECK_BLIND_SPOTS);
    assert_eq!(head[7], "Rules not run: PM501: needs `analysis.dupes = true`");
    // No findings: the header only.
    let empty = render(&Findings::new(), &llm(50), &Summary::default());
    assert!(empty.contains("\nNo findings · exit 0 = clean\n"), "{empty}");
    assert!(!empty.contains("Next:"), "{empty}");
}

#[test]
fn deterministic() {
    let f = chain();
    assert_eq!(render(&f, &llm(0), &Summary::default()), render(&f, &llm(0), &Summary::default()));
}
