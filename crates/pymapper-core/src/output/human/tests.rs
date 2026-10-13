//! Unit tests of the human format: merging, pairs once, section order, ids, tally, colour.
//! (Whole-demo goldens are in `crates/pymapper-cli/tests/human.rs`.)

use super::super::tests::TestCtx;
use super::super::{Options, Out, OutputFormat, Report, Summary};
use super::{DEFAULT_WIDTH, GroupBy, write_with_width};
use crate::rules::codes::*;
use crate::rules::{Arg, FileId, FilterStats, Findings, Location, Rule, SymbolId};

fn ctx() -> TestCtx {
    TestCtx {
        paths: vec![b"pkg/zeta.py".to_vec(), b"pkg/alpha.py".to_vec()],
        symbols: vec!["pkg.zeta:handle", "pkg.alpha", "pkg.alpha:Big.run", "pkg.zeta", "pkg.alpha:helper_with_a_long_name"],
        names: vec!["os"],
        contracts: vec!["core is independent"],
    }
}

fn loc(file: u32, line: u32, col: u32) -> Location {
    Location::new(FileId(file), line, col)
}

fn metric(f: &mut Findings, rule: Rule, l: Location, s: u32, v: u32, t: u32) {
    f.push(rule, l, SymbolId(s), 100, &[Arg::Symbol(SymbolId(s)), Arg::U32(v), Arg::U32(t)]);
}

/// One finding of most families, pushed in code order (output order must not follow it).
fn sample() -> Findings {
    let mut f = Findings::new();
    let s = SymbolId;
    metric(&mut f, COMPLEX_FUNCTION, loc(0, 12, 5), 0, 14, 10);
    metric(&mut f, COGNITIVE_COMPLEXITY, loc(0, 12, 5), 0, 20, 15);
    metric(&mut f, TOO_MANY_PARAMETERS, loc(0, 12, 5), 0, 9, 7);
    f.push(HIGH_FAN_OUT, loc(1, 1, 1), s(1), 100, &[Arg::Symbol(s(1)), Arg::U32(12), Arg::U32(10)]);
    let cycle = [Arg::U32(2), Arg::Phrase(1), Arg::Symbol(s(1)), Arg::Symbol(s(3)), Arg::Symbol(s(1))];
    f.push_ext(IMPORT_CYCLE, loc(1, 2, 1), 0, SymbolId::NONE, 100, &cycle, &[Arg::Symbol(s(1)), Arg::Symbol(s(3))]);
    f.push(FORBIDDEN_IMPORT, loc(0, 3, 1), s(3), 100, &[Arg::Symbol(s(3)), Arg::Name(0), Arg::Contract(0)]);
    // A duplicate pair: one finding per copy.
    let a = [Arg::U32(8), Arg::File(FileId(1)), Arg::U32(40)];
    f.push_ext(DUPLICATE_CODE, loc(0, 14, 1), 21, s(0), 98, &a, &[Arg::File(FileId(1)), Arg::U32(40), Arg::U32(47), Arg::Symbol(s(2))]);
    let b = [Arg::U32(8), Arg::File(FileId(0)), Arg::U32(14)];
    f.push_ext(DUPLICATE_CODE, loc(1, 40, 1), 47, s(2), 98, &b, &[Arg::File(FileId(0)), Arg::U32(14), Arg::U32(21), Arg::Symbol(s(0))]);
    f.push(UNUSED_FUNCTION, loc(1, 30, 5), s(4), 100, &[Arg::Symbol(s(4)), Arg::Phrase(0)]);
    let decorated = crate::rules::dead_code::reason::UNKNOWN_DECORATOR;
    f.push_with_reasons(UNUSED_METHOD, loc(1, 20, 9), s(2), 40, &[Arg::Symbol(s(2)), Arg::Phrase(7)], decorated);
    f.push(UNUSED_IMPORT, loc(0, 1, 8), s(3), 100, &[Arg::Name(0)]);
    f
}

fn summary() -> Summary<'static> {
    Summary { files_checked: 9, filter: FilterStats { reported: 11, below_confidence: 3, disabled: 0, file_ignored: 1, suppressed: 2 }, ..Summary::default() }
}

fn render_with(findings: &Findings, group_by: GroupBy, color: bool, width: usize) -> String {
    let c = ctx();
    let r = Report::new(findings, &c);
    let opts = Options { group_by, color, ..Options::new(OutputFormat::Human) };
    let mut sink = Vec::new();
    let mut out = Out::new(&mut sink);
    write_with_width(&r, &opts, &summary(), width, &mut out);
    out.finish().expect("write");
    String::from_utf8(sink).expect("utf-8")
}

fn render(findings: &Findings) -> String {
    render_with(findings, GroupBy::Category, false, DEFAULT_WIDTH)
}

#[test]
fn category_layout() {
    let got = render(&sample());
    let want = "Architecture  2\n  pkg/alpha.py\n     2:1   import cycle (2 modules): pkg.alpha -> pkg.zeta -> pkg.alpha   ";
    assert!(got.starts_with(want), "{got}");
    // Sections by priority, not by code.
    let order = ["Complexity  1 function", "Coupling  1", "Duplication  1 pair", "Dead code  2", "Unused imports  1"];
    let mut at = 0;
    for h in order {
        let i = got[at..].find(&format!("\n\n{h}\n"));
        at += i.unwrap_or_else(|| panic!("{h} missing or out of order:\n{got}")) + 1;
    }
    assert!(got.contains("\n  pkg/zeta.py\n     3:1   imports forbidden os (contract: core is independent)"), "{got}");
    assert!(got.contains(
        "\n    20:9   method    Big.run                  has no resolved callers (an unknown decorator may register it) \
         (confidence 40: unknown-decorator)  PM403-"
    ), "{got}");
    assert!(got.contains("\n    30:5   function  helper_with_a_long_name  never used"), "{got}");
    assert!(got.contains("\n     1:8   os   "), "{got}");
    assert!(!got.contains('`'), "{got}");
}

#[test]
fn metrics_merge_into_one_line_with_one_id() {
    let got = render(&sample());
    let line = got.lines().find(|l| l.contains("McCabe")).expect("metric line");
    assert!(line.starts_with("    12:5   handle  McCabe 14>10 · cognitive 20>15 · params 9>7"), "{line}");
    assert!(line.ends_with(" +PM102,PM105"), "{line}");
    assert!(line.contains("  PM101-"), "{line}");
    assert_eq!(got.matches("McCabe").count(), 1);
    // Different symbols do not merge.
    let mut f = Findings::new();
    metric(&mut f, COMPLEX_FUNCTION, loc(0, 12, 5), 0, 14, 10);
    metric(&mut f, COMPLEX_FUNCTION, loc(0, 30, 5), 4, 11, 10);
    let got = render(&f);
    assert!(got.contains("Complexity  2 functions\n"), "{got}");
    assert_eq!(got.matches("McCabe").count(), 2, "{got}");
    // A module metric counts as a module; its module id is kept whole.
    metric(&mut f, LONG_MODULE, loc(1, 1, 1), 1, 900, 500);
    let got = render(&f);
    assert!(got.contains("Complexity  2 functions, 1 module\n"), "{got}");
    assert!(got.contains("pkg.alpha                lines 900>500"), "{got}");
}

#[test]
fn a_duplicate_pair_is_shown_once_with_both_locations() {
    let got = render(&sample());
    assert_eq!(got.matches('↔').count(), 1, "{got}");
    assert!(got.contains("\n  pkg/alpha.py:40  Big.run  ↔  pkg/zeta.py:14  handle  (8 lines) (confidence 98)"), "{got}");
    let got = render_with(&sample(), GroupBy::File, false, DEFAULT_WIDTH);
    assert_eq!(got.matches('↔').count(), 1, "{got}");
    assert!(got.contains("\n    40:1   duplication    Big.run  ↔  pkg/zeta.py:14  handle  (8 lines)"), "{got}");
}

#[test]
fn file_layout() {
    let got = render_with(&sample(), GroupBy::File, false, DEFAULT_WIDTH);
    // pkg/alpha.py sorts before pkg/zeta.py; findings by line.
    assert!(got.starts_with("pkg/alpha.py\n     1:1   coupling       pkg.alpha  fan-out 12>10"), "{got}");
    let zeta = got.find("\n\npkg/zeta.py\n").expect("zeta");
    assert!(got.contains("\n    30:5   dead code      function helper_with_a_long_name  never used"), "{got}");
    let first = got[zeta + 2..].lines().nth(1).expect("first zeta line");
    assert!(first.starts_with("     1:8   unused import  os"), "{first}");
    assert!(got.contains("\n    12:5   complexity     handle  McCabe 14>10"), "{got}");
}

#[test]
fn ids_are_right_aligned_when_they_fit() {
    let got = render_with(&sample(), GroupBy::Category, false, 100);
    let line = got.lines().find(|l| l.contains("PM405-")).expect("import line");
    assert_eq!(line.chars().count(), 100, "{line:?}");
    // Too long for the width: two spaces, nothing cut.
    let narrow = render_with(&sample(), GroupBy::Category, false, 40);
    let line = narrow.lines().find(|l| l.contains("unknown decorator")).expect("long line");
    assert!(line.contains("unknown-decorator)  PM403-"), "{line}");
    let line = narrow.lines().find(|l| l.contains("PM405-")).expect("import line");
    assert_eq!(line.chars().count(), 40, "{line:?}");
}

#[test]
fn footer_tallies_sections_and_notes() {
    let got = render(&sample());
    let tail: Vec<&str> = got.lines().rev().take(3).collect();
    assert_eq!(
        tail[2],
        "11 findings in 2 files (1 below confidence 80; 9 files checked) · architecture 2 · complexity 1 function · coupling 1 · \
         duplication 1 pair · dead code 2 · unused imports 1"
    );
    assert_eq!(tail[1], "2 more below confidence 80 (use --min-confidence 0 to show them)");
    assert_eq!(tail[0], "Not shown: 2 suppressed, 1 ignored by per-file-ignores.");
    let empty = render(&Findings::new());
    assert!(empty.starts_with("All checks passed! (9 files checked)\n"), "{empty}");
}

#[test]
fn colour_only_when_asked() {
    assert!(!render(&sample()).contains('\x1b'));
    let got = render_with(&sample(), GroupBy::Category, true, DEFAULT_WIDTH);
    assert!(got.starts_with("\x1b[1;31mArchitecture\x1b[0m  2\n  \x1b[1mpkg/alpha.py\x1b[0m\n"), "{got:?}");
    assert!(got.contains("\x1b[1;35mComplexity\x1b[0m"), "{got:?}");
    assert!(got.contains("\x1b[2m(confidence 40: unknown-decorator)\x1b[0m"), "{got:?}");
    // Escapes do not count towards the id alignment.
    let line = got.lines().find(|l| l.contains("PM405-")).expect("import line");
    assert_eq!(super::super::style::width(line.as_bytes()), DEFAULT_WIDTH, "{line:?}");
}
