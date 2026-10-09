//! Tests of coverage evidence: report parsing, path matching, line-to-symbol mapping and the
//! effect on dead-code levels and findings. Reports are synthetic `coverage json` documents.

use std::path::Path;

use super::coverage::{Coverage, CoverageError};
use super::evidence_tests::{app_opts, collect, find};
use super::tests::{Fx, fx};
use super::*;
use crate::rules::dead_code::reason;

/// A coverage.py format-3 report with `entries` (path, executed lines, missing lines), plus
/// the keys a real report has around them (skipped by the reader).
fn report(entries: &[(&str, &[u32], &[u32])]) -> String {
    let mut s = String::from(
        "{\"meta\": {\"format\": 3, \"version\": \"7.6.1\", \"timestamp\": \"2026-10-09T10:00:00\", \
         \"branch_coverage\": false, \"show_contexts\": false},\n \"files\": {",
    );
    for (i, (path, exec, miss)) in entries.iter().enumerate() {
        if i > 0 {
            s.push_str(", ");
        }
        let list = |l: &[u32]| l.iter().map(u32::to_string).collect::<Vec<_>>().join(", ");
        s.push_str(&format!(
            "\"{path}\": {{\"executed_lines\": [{}], \"summary\": {{\"covered_lines\": {}, \"percent_covered\": 50.0, \
             \"percent_covered_display\": \"50\"}}, \"missing_lines\": [{}], \"excluded_lines\": [], \
             \"functions\": {{\"f\": {{\"executed_lines\": [999], \"missing_lines\": []}}}}, \"classes\": {{}}, \
             \"contexts\": {{\"1\": [\"\"]}}}}",
            list(exec),
            exec.len(),
            list(miss)
        ));
    }
    s.push_str("},\n \"totals\": {\"covered_lines\": 1, \"num_statements\": 2}}\n");
    s
}

/// Absolute path of the file of module `qid`.
fn path_of(f: &Fx, qid: &str) -> String {
    let file = f.ix.file(f.node(qid)).expect("file");
    f.ix.file_path(file).to_str().expect("utf-8").to_owned()
}

fn parse(f: &Fx, json: &str) -> Coverage {
    Coverage::parse(&f.ix, json.as_bytes(), &[]).unwrap_or_else(|e| panic!("{e}"))
}

/// `"executed"`, `"not"` or `"unknown"` for node `qid`.
fn state(f: &Fx, c: &Coverage, qid: &str) -> &'static str {
    let v = f.node(qid);
    match (c.executed(v), c.not_executed(v)) {
        (true, false) => "executed",
        (false, true) => "not",
        (false, false) => "unknown",
        (true, true) => panic!("{qid} is both executed and not executed"),
    }
}

const LIB: &str = "FLAG = False\n\
\n\
def called():\n\
\x20   return 1\n\
\n\
def never():\n\
\x20   return 2\n\
\n\
def one(): return 3\n\
\n\
if FLAG:\n\
\x20   def tiny(): pass\n\
\n\
class Data:\n\
\x20   x = 1\n\
\n\
class Unused:\n\
\x20   def a(self):\n\
\x20       pass\n\
\n\
\x20   def b(self):\n\
\x20       pass\n\
\n\
class Mixed:\n\
\x20   def a(self):\n\
\x20       pass\n\
\n\
\x20   def b(self):\n\
\x20       pass\n\
\n\
def excluded():\n\
\x20   pass\n\
\n\
def outer():\n\
\x20   def inner():\n\
\x20       return 1\n\
\x20   return inner\n";

const LIB_EXEC: &[u32] = &[1, 3, 4, 6, 9, 11, 14, 15, 17, 18, 21, 24, 25, 26, 28, 31, 34, 35, 37];
const LIB_MISS: &[u32] = &[7, 12, 19, 22, 29, 36];

fn lib_fx() -> Fx {
    fx(&[("lib.py", LIB), ("other.py", "def f():\n    pass\n\nclass K:\n    pass\n"), ("absent.py", "def g():\n    pass\n")])
}

#[test]
fn lines_map_to_defs_and_modules() {
    let f = lib_fx();
    let json = report(&[(&path_of(&f, "lib"), LIB_EXEC, LIB_MISS), (&path_of(&f, "other"), &[], &[1, 2, 4, 5])]);
    let c = parse(&f, &json);
    let expect = [
        ("lib", "executed"),
        ("lib:called", "executed"),
        ("lib:never", "not"),
        // The `def` line runs at import: a one-liner's body cannot be told apart.
        ("lib:one", "unknown"),
        // Its `def` statement never ran.
        ("lib:tiny", "not"),
        // A class body runs at import; no methods, no evidence.
        ("lib:Data", "unknown"),
        ("lib:Unused", "not"),
        ("lib:Unused.a", "not"),
        ("lib:Unused.b", "not"),
        // A live method keeps the class through propagation, not through the report.
        ("lib:Mixed", "unknown"),
        ("lib:Mixed.a", "executed"),
        ("lib:Mixed.b", "not"),
        // `# pragma: no cover`: in neither list.
        ("lib:excluded", "unknown"),
        ("lib:outer", "executed"),
        ("lib:outer.inner", "not"),
        // Listed with missing lines only: never imported.
        ("other", "not"),
        ("other:f", "not"),
        ("other:K", "not"),
        // Not in the report: no evidence.
        ("absent", "unknown"),
        ("absent:g", "unknown"),
    ];
    for (qid, want) in expect {
        assert_eq!(state(&f, &c, qid), want, "{qid}");
    }
    let s = c.stats();
    assert_eq!((s.report_files, s.matched, s.suffix_matched, s.stale_files), (2, 2, 0, 0));
    assert_eq!((s.executed, s.not_executed), (4, 10));
    assert!(c.first_unmatched().is_none());
    assert_eq!(c.node_count(), f.ix.node_count());
}

#[test]
fn unsorted_duplicate_and_stale_lines() {
    let f = lib_fx();
    let lib = path_of(&f, "lib");
    let mut exec = LIB_EXEC.to_vec();
    exec.reverse();
    // A line past the end of `other.py` (5 lines): a stale report.
    let json = report(&[(&lib, &exec, LIB_MISS), (&path_of(&f, "other"), &[], &[1, 2, 40])]);
    let c = parse(&f, &json);
    assert_eq!(state(&f, &c, "lib:called"), "executed");
    assert_eq!(state(&f, &c, "lib:never"), "not");
    assert_eq!(c.stats().stale_files, 1);
    // The same file twice, the second entry contradicting the first: executed wins.
    let json = report(&[(&lib, &[1, 3, 6, 7], &[4]), (&lib, &[1, 3, 6], &[7])]);
    let c = parse(&f, &json);
    assert_eq!(state(&f, &c, "lib:never"), "executed");
    assert_eq!(state(&f, &c, "lib:called"), "not");
}

#[test]
fn relative_paths_join_the_base_directories() {
    let f = lib_fx();
    let lib = path_of(&f, "lib");
    let dir = Path::new(&lib).parent().expect("dir").to_owned();
    let name = dir.file_name().expect("name").to_str().expect("utf-8").to_owned();
    let json = report(&[("lib.py", LIB_EXEC, LIB_MISS), ("./sub/../other.py", &[], &[1])]);
    // Neither base matches first; the second does.
    let c = Coverage::parse(&f.ix, json.as_bytes(), &[Path::new("/nonexistent"), &dir]).expect("parse");
    assert_eq!(state(&f, &c, "lib:called"), "executed");
    assert_eq!(state(&f, &c, "other"), "not");
    assert_eq!(c.stats().suffix_matched, 0);
    // Relative to the parent directory, with an escaped slash.
    let parent = dir.parent().expect("parent");
    let key = format!("{name}\\/lib.py");
    let json = report(&[(&key, LIB_EXEC, LIB_MISS)]);
    let c = Coverage::parse(&f.ix, json.as_bytes(), &[parent]).expect("parse");
    assert_eq!(state(&f, &c, "lib:called"), "executed");
    assert_eq!(c.stats().suffix_matched, 0);
}

#[test]
fn paths_of_another_checkout_match_by_suffix() {
    let f = fx(&[
        ("pkg/__init__.py", ""),
        ("pkg/mod.py", "def f():\n    pass\n"),
        ("pkg/sub/__init__.py", ""),
        ("pkg/sub/mod.py", "def g():\n    pass\n"),
        ("solo.py", "def h():\n    pass\n"),
    ]);
    let json = report(&[
        // CI checkout: directory and name match.
        ("/home/runner/work/proj/proj/pkg/mod.py", &[1, 2], &[]),
        // Windows separators.
        ("C:\\\\build\\\\pkg\\\\sub\\\\mod.py", &[1], &[2]),
        // `__init__.py` under two packages, only the name matches: ambiguous.
        ("/elsewhere/__init__.py", &[], &[1]),
        // A unique name, but only one component of a longer path matches: not trusted.
        ("/other/project/solo.py", &[1], &[2]),
        // A one-component relative key matches on its own.
        ("solo.py", &[1], &[2]),
        ("/nowhere/unknown.py", &[1], &[]),
    ]);
    let c = parse(&f, &json);
    assert_eq!(state(&f, &c, "pkg.mod:f"), "executed");
    assert_eq!(state(&f, &c, "pkg.sub.mod:g"), "not");
    assert_eq!(state(&f, &c, "pkg"), "unknown");
    assert_eq!(state(&f, &c, "solo:h"), "not");
    let s = c.stats();
    assert_eq!((s.report_files, s.matched, s.suffix_matched), (6, 3, 3));
    assert_eq!(c.first_unmatched(), Some(&b"/elsewhere/__init__.py"[..]));
}

#[test]
fn malformed_reports_are_errors() {
    let f = lib_fx();
    let not_cov = |json: &str| matches!(Coverage::parse(&f.ix, json.as_bytes(), &[]), Err(CoverageError::NotCoverage));
    assert!(not_cov("{}"));
    assert!(not_cov("{\"meta\": {\"format\": 3}, \"files\": []}"));
    assert!(not_cov("{\"totals\": {}}"));
    let bad_line = format!("{{\"files\": {{\"{}\": {{\"executed_lines\": [1, x]}}}}}}", path_of(&f, "lib"));
    for bad in ["", "[]", "{\"files\": {", &bad_line, "{\"files\": {}} trailing"] {
        match Coverage::parse(&f.ix, bad.as_bytes(), &[]) {
            Err(CoverageError::Json { .. }) => {}
            other => panic!("{bad:?}: {other:?}"),
        }
    }
    // Unmatched files are not checked: a non-integer line there is skipped.
    let c = parse(&f, "{\"files\": {\"nope.py\": {\"executed_lines\": [\"x\"]}, \"bad\": 3}}");
    assert_eq!(c.stats().matched, 0);
    assert_eq!(c.stats().report_files, 2);
    let err = Coverage::load(&f.ix, Path::new("/nonexistent/coverage.json"), Path::new("/")).expect_err("missing");
    assert!(matches!(err, CoverageError::Read(_)), "{err}");
}

const APP: &str = "from lib import helper\n\
\n\
def entry():\n\
\x20   helper()\n\
\n\
def ghost():\n\
\x20   pass\n\
\n\
def maybe():\n\
\x20   pass\n\
\n\
def live():\n\
\x20   pass\n\
\n\
if __name__ == '__main__':\n\
\x20   live()\n";

fn app_fx() -> Fx {
    fx(&[
        ("app.py", APP),
        ("lib.py", "def helper():\n    return 1\n\ndef untested():\n    return 2\n"),
        ("notes.py", "NAME = 'maybe'\n"),
        ("tool.py", "def run():\n    pass\n"),
    ])
}

fn app_report(f: &Fx) -> String {
    report(&[
        // `entry` ran (a dynamic caller the analysis cannot see); nothing else did.
        (&path_of(f, "app"), &[1, 3, 4, 6, 9, 12, 15], &[7, 10, 13, 16]),
        // `helper`'s body is missing: reached from executed code, it stays live.
        (&path_of(f, "lib"), &[1, 4], &[2, 5]),
        (&path_of(f, "notes"), &[1], &[]),
        (&path_of(f, "tool"), &[], &[1, 2]),
    ])
}

#[test]
fn coverage_raises_executed_code_and_lowers_code_that_never_ran() {
    let f = app_fx();
    let (_, before) = collect(&f, &app_opts());
    let conf = |rows: &[super::evidence_tests::Row], q: &str| find(rows, q).map(|r| r.conf);
    assert_eq!(conf(&before, "app:entry"), Some(100));
    assert_eq!(conf(&before, "lib:helper"), Some(100));
    assert_eq!(conf(&before, "app:maybe"), Some(50));
    assert_eq!(conf(&before, "tool"), Some(40));
    assert_eq!(conf(&before, "notes"), Some(40));

    let c = parse(&f, &app_report(&f));
    let opts = app_opts().with_coverage(Some(&c));
    let (dc, rows) = collect(&f, &opts);
    // Executed: live, and so is what it calls.
    assert_eq!(find(&rows, "app:entry"), None, "{rows:?}");
    assert_eq!(dc.why(f.node("app:entry")), Why::Executed);
    assert_eq!(find(&rows, "lib:helper"), None);
    assert_eq!(dc.why(f.node("lib:helper")), Why::Call);
    // Statically live but never executed (untested): unchanged.
    assert_eq!(find(&rows, "app:live"), None);
    assert_eq!(dc.level(f.node("app:live")), level::LIVE);
    // Unreachable and never executed: still 100, with the reason.
    let ghost = find(&rows, "app:ghost").expect("ghost");
    assert_eq!(ghost.conf, 100);
    assert_eq!(ghost.reasons, reason::NO_REFERENCES | reason::NEVER_EXECUTED);
    assert_eq!(find(&rows, "lib:untested").map(|r| r.conf), Some(100));
    // Weak evidence of use, never executed: 100 - 50 / 3.
    let maybe = find(&rows, "app:maybe").expect("maybe");
    assert_eq!(maybe.conf, 84);
    assert_eq!(maybe.reasons, reason::NAME_ELSEWHERE | reason::NEVER_EXECUTED);
    assert_eq!(dc.origin(f.node("app:maybe")), Why::Evidence);
    // Modules: an executed script-like module is live; one never imported loses its doubt.
    assert_eq!(find(&rows, "notes"), None);
    assert_eq!(dc.module_why(f.node("notes")), Some(Why::Executed));
    let tool = find(&rows, "tool").expect("tool");
    assert_eq!(tool.conf, 80);
    assert_eq!(tool.reasons, reason::SCRIPT_LIKE | reason::NEVER_EXECUTED);
    // Its function is not contained in the module finding (higher confidence).
    let run = find(&rows, "tool:run").expect("run");
    assert_eq!((run.conf, run.reasons), (100, reason::NO_REFERENCES | reason::NEVER_EXECUTED));
    let s = dc.stats();
    // Seeds: entry and 3 modules; lowered: maybe and tool.
    assert_eq!((s.executed, s.not_executed), (4, 2));
}

#[test]
fn library_public_names_that_never_ran() {
    let f = fx(&[
        ("pkg/__init__.py", "from pkg.api import api\n__all__ = ['api']\n"),
        ("pkg/api.py", "from pkg._impl import ran\n\ndef api():\n    ran()\n"),
        ("pkg/_impl.py", "def public_helper():\n    pass\n\ndef ran():\n    pass\n"),
    ]);
    let lib_opts = DeadCodeOptions { mode: Mode::Library, ..DeadCodeOptions::default() };
    let (_, before) = collect(&f, &lib_opts);
    assert_eq!(find(&before, "pkg._impl:public_helper").map(|r| r.conf), Some(80), "{before:?}");
    let json = report(&[(&path_of(&f, "pkg._impl"), &[1, 4, 5], &[2])]);
    let c = parse(&f, &json);
    let (_, rows) = collect(&f, &lib_opts.with_coverage(Some(&c)));
    let r = find(&rows, "pkg._impl:public_helper").expect("public_helper");
    assert_eq!(r.conf, 94);
    assert_eq!(r.reasons, reason::LIBRARY_PUBLIC_NOT_IN_ALL | reason::NEVER_EXECUTED);
    assert_eq!(find(&rows, "pkg._impl:ran"), None);
}

#[test]
fn coverage_of_another_index_is_ignored() {
    let f = app_fx();
    let other = lib_fx();
    let c = parse(&other, &report(&[(&path_of(&other, "lib"), LIB_EXEC, LIB_MISS)]));
    assert_ne!(c.node_count(), f.ix.node_count());
    let (_, plain) = collect(&f, &app_opts());
    let (dc, rows) = collect(&f, &app_opts().with_coverage(Some(&c)));
    assert_eq!(rows, plain);
    assert_eq!((dc.stats().executed, dc.stats().not_executed), (0, 0));
}

#[test]
fn reason_name_is_rendered() {
    let names = reason::NAMES;
    assert_eq!(names[reason::NEVER_EXECUTED.trailing_zeros() as usize], "never-executed");
    assert_eq!(Why::from_u8(Why::Executed as u8), Why::Executed);
    assert_eq!(Why::Executed.as_str(), "executed");
}
