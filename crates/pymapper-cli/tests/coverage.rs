//! End-to-end tests of coverage evidence (`--coverage`, `analysis.coverage`) in `pymapper
//! check`: the real binary on a small project with a synthetic `coverage json` report.

use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};

/// A throwaway project directory, removed on drop.
struct Project {
    dir: PathBuf,
}

impl Project {
    fn new(name: &str) -> Project {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("pymapper-cov-{}-{n}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create project dir");
        let dir = dir.canonicalize().expect("canonicalize");
        let p = Project { dir };
        p.file("pyproject.toml", "[project]\nname = \"app\"\n")
            .file("app/__init__.py", "")
            .file(
                "app/main.py",
                "from app import registry\nfrom app.util import helper\n\n\ndef run():\n    return helper() + len(registry.NAMES)\n\n\n\
                 if __name__ == '__main__':\n    run()\n",
            )
            .file("app/registry.py", "NAMES = ['maybe']\n")
            .file("app/util.py", "def helper():\n    return 1\n\n\ndef plugin():\n    return 2\n\n\ndef maybe():\n    return 3\n")
            .file(
                "coverage.json",
                "{\"meta\": {\"format\": 3, \"version\": \"7.6.1\"}, \"files\": {\"app/util.py\": \
                 {\"executed_lines\": [1, 2, 5, 6, 9], \"missing_lines\": [10], \"excluded_lines\": []}}, \"totals\": {}}\n",
            );
        p
    }

    fn file(&self, rel: &str, text: &str) -> &Project {
        let p = self.dir.join(rel);
        std::fs::create_dir_all(p.parent().expect("parent")).expect("create parents");
        std::fs::write(&p, text).expect("write file");
        self
    }

    /// Runs `pymapper check --no-cache --select PM4 <args>` in the project directory: (exit
    /// code, stdout, stderr).
    fn check(&self, args: &[&str]) -> (i32, String, String) {
        let out = Command::new(env!("CARGO_BIN_EXE_pymapper"))
            .arg("check")
            .args(["--no-cache", "--select", "PM4"])
            .args(args)
            .current_dir(&self.dir)
            .env_remove("VIRTUAL_ENV")
            .output()
            .expect("run pymapper");
        let s = |b: Vec<u8>| String::from_utf8(b).expect("utf-8");
        (out.status.code().expect("exit code"), s(out.stdout), s(out.stderr))
    }
}

impl Drop for Project {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

const PLUGIN: &str = "app/util.py:5:5: PM401 function `app.util:plugin` is never used";
const MAYBE: &str = "app/util.py:9:5: PM401 function `app.util:maybe` has no resolved callers (name appears elsewhere) \
                     (confidence 84: name-elsewhere, never-executed)";

#[test]
fn coverage_flag_suppresses_executed_code_and_raises_code_that_never_ran() {
    let p = Project::new("flag");
    let (code, out, err) = p.check(&[]);
    assert_eq!(code, 1, "{out}{err}");
    assert!(out.contains(PLUGIN), "{out}");
    assert!(!out.contains("app.util:maybe"), "{out}");

    let (code, out, err) = p.check(&["--coverage", "coverage.json"]);
    assert_eq!(code, 1, "{out}{err}");
    assert!(!out.contains("app.util:plugin"), "{out}");
    assert!(out.contains(MAYBE), "{out}");
    assert!(!err.contains("warning"), "{err}");

    let (_, _, err) = p.check(&["--coverage", "coverage.json", "--stats"]);
    for key in ["coverage_files", "coverage_matched", "coverage_executed", "coverage_not_executed", "deadcode.executed"] {
        assert!(err.contains(key), "{key} missing in\n{err}");
    }
}

#[test]
fn coverage_from_the_config_file_and_no_coverage() {
    let p = Project::new("config");
    p.file("pymapper.toml", "[analysis]\ncoverage = \"coverage.json\"\n");
    let (code, out, err) = p.check(&[]);
    assert_eq!(code, 1, "{out}{err}");
    assert!(out.contains(MAYBE), "{out}");
    assert!(!out.contains("app.util:plugin"), "{out}");
    let (_, out, _) = p.check(&["--no-coverage"]);
    assert!(out.contains(PLUGIN), "{out}");
    // The flag wins over the config file.
    let (code, _, err) = p.check(&["--coverage", "other.json"]);
    assert_eq!(code, 2, "{err}");
}

#[test]
fn bad_reports_are_errors_or_warnings() {
    let p = Project::new("bad");
    let (code, _, err) = p.check(&["--coverage", "missing.json"]);
    assert_eq!(code, 2);
    assert!(err.contains("cannot read coverage report") && err.contains("missing.json"), "{err}");

    p.file("broken.json", "{\"files\": {\"app/util.py\": {\"executed_lines\": [1,");
    let (code, _, err) = p.check(&["--coverage", "broken.json"]);
    assert_eq!(code, 2);
    assert!(err.contains("invalid coverage report") && err.contains("invalid JSON"), "{err}");

    p.file("other.json", "{\"meta\": {}}");
    let (code, _, err) = p.check(&["--coverage", "other.json"]);
    assert_eq!(code, 2);
    assert!(err.contains("not a coverage.py JSON report"), "{err}");

    // A report of another project: a warning, and the findings without coverage.
    p.file("foreign.json", "{\"files\": {\"/somewhere/else/thing.py\": {\"executed_lines\": [1], \"missing_lines\": []}}}");
    let (code, out, err) = p.check(&["--coverage", "foreign.json"]);
    assert_eq!(code, 1);
    assert!(err.contains("matches no analysed file (first path: /somewhere/else/thing.py)"), "{err}");
    assert!(out.contains(PLUGIN), "{out}");

    // Lines past the end of a file: stale.
    p.file("stale.json", "{\"files\": {\"app/util.py\": {\"executed_lines\": [1, 2, 50], \"missing_lines\": []}}}");
    let (_, _, err) = p.check(&["--coverage", "stale.json"]);
    assert!(err.contains("1 file(s) have line numbers past their end"), "{err}");

    // Dead code not selected: the report is not read at all.
    let out = Command::new(env!("CARGO_BIN_EXE_pymapper"))
        .args(["check", "--no-cache", "--select", "PM1", "--coverage", "missing.json"])
        .current_dir(&p.dir)
        .env_remove("VIRTUAL_ENV")
        .output()
        .expect("run pymapper");
    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stderr));
}

/// One file entry of a coverage.py 7.5+ report: `executed` / `missing` lines plus the
/// `functions` and `classes` sections, which name every def (`(name, executed, missing)`).
fn file_entry(executed: &[u32], missing: &[u32], functions: &[(&str, &[u32], &[u32])]) -> String {
    let lines = |l: &[u32]| l.iter().map(|n| n.to_string()).collect::<Vec<_>>().join(", ");
    let fields = |e: &[u32], m: &[u32]| {
        format!(
            "\"executed_lines\": [{}], \"summary\": {{\"covered_lines\": {}, \"num_statements\": {}, \
             \"percent_covered\": 0.0, \"percent_covered_display\": \"0\", \"missing_lines\": {}, \
             \"excluded_lines\": 0}}, \"missing_lines\": [{}], \"excluded_lines\": []",
            lines(e),
            e.len(),
            e.len() + m.len(),
            m.len(),
            lines(m)
        )
    };
    let mut fns = String::new();
    for (name, e, m) in functions {
        fns.push_str(&format!("\"{name}\": {{{}}}, ", fields(e, m)));
    }
    fns.push_str(&format!("\"\": {{{}}}", fields(&[], &[])));
    let whole = fields(executed, missing);
    format!("{{{whole}, \"functions\": {{{fns}}}, \"classes\": {{\"\": {{{whole}}}}}}}")
}

/// A coverage.py 7.15 report of the test project, as written by `coverage json`.
fn realistic_report() -> String {
    let util = file_entry(&[1, 2, 5, 9], &[6, 10], &[("helper", &[2], &[]), ("plugin", &[], &[6]), ("maybe", &[], &[10])]);
    let main = file_entry(&[1, 2, 5, 6, 9, 10], &[], &[("run", &[6], &[])]);
    let registry = file_entry(&[1], &[], &[]);
    let init = file_entry(&[], &[], &[]);
    format!(
        "{{\"meta\": {{\"format\": 3, \"version\": \"7.15.2\", \"timestamp\": \"2026-10-09T10:00:00.000000\", \
         \"branch_coverage\": false, \"show_contexts\": false}}, \"files\": {{\"app/__init__.py\": {init}, \
         \"app/main.py\": {main}, \"app/registry.py\": {registry}, \"app/util.py\": {util}}}, \
         \"totals\": {{\"covered_lines\": 11, \"num_statements\": 13, \"percent_covered\": 84.6, \
         \"percent_covered_display\": \"85\", \"missing_lines\": 2, \"excluded_lines\": 0}}}}\n"
    )
}

#[test]
fn a_realistic_report_in_the_project_is_not_config_evidence() {
    let p = Project::new("realistic");
    p.file("coverage.json", &realistic_report());
    // Without `--coverage` the report is recognised as tool output: `plugin` is still never
    // used (no `config-reference`).
    let (code, out, err) = p.check(&["--min-confidence", "0"]);
    assert_eq!(code, 1, "{out}{err}");
    assert!(out.contains(PLUGIN), "{out}");
    assert!(!out.contains("config-reference"), "{out}");

    // With it, the never-executed weak-evidence def is reported at 84.
    let (code, out, err) = p.check(&["--coverage", "coverage.json"]);
    assert_eq!(code, 1, "{out}{err}");
    assert!(out.contains(MAYBE), "{out}");
    assert!(out.contains(PLUGIN), "{out}");
    assert!(!out.contains("config-reference"), "{out}");
}

#[test]
fn the_configured_report_is_never_config_evidence() {
    let p = Project::new("by-path");
    // `files` before `meta`: the head does not look like coverage.py output, so the report is
    // left out only because it is the configured one.
    let report = realistic_report();
    let at = report.find(", \"files\"").expect("files");
    let reordered = format!("{{{}, {}}}\n", &report[at + 2..report.trim_end().len() - 1], &report[1..at]);
    p.file("cov-report.json", &reordered);
    let (_, out, _) = p.check(&["--min-confidence", "0"]);
    assert!(out.contains("config-reference"), "the report is config evidence without --coverage:\n{out}");

    let (code, out, err) = p.check(&["--coverage", "cov-report.json", "--min-confidence", "0"]);
    assert_eq!(code, 1, "{out}{err}");
    assert!(out.contains("maybe") && out.contains("name-elsewhere, never-executed"), "{out}");
    assert!(!out.contains("config-reference"), "{out}");
    // Through another path to the same file.
    let (_, out, _) = p.check(&["--coverage", "app/../cov-report.json", "--min-confidence", "0"]);
    assert!(out.contains("name-elsewhere, never-executed") && !out.contains("config-reference"), "{out}");
}
