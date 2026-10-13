//! End-to-end tests of `pymapper deps`: the real binary on a small project with a venv.

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
        let dir = std::env::temp_dir().join(format!("pymapper-deps-e2e-{}-{n}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create project dir");
        let dir = dir.canonicalize().expect("canonicalize");
        Project { dir }
    }

    fn file(&self, rel: &str, text: &str) -> &Project {
        let p = self.dir.join(rel);
        std::fs::create_dir_all(p.parent().expect("parent")).expect("create parents");
        std::fs::write(&p, text).expect("write file");
        self
    }

    /// A distribution in the venv: `files` (relative to site-packages, with sources) are listed
    /// in its `RECORD`, plus `extra` listed files that are not written (compiled modules).
    fn dist(&self, name: &str, version: &str, files: &[(&str, &str)], extra: &[&str]) -> &Project {
        let site = ".venv/lib/python3.12/site-packages";
        let info = format!("{site}/{name}-{version}.dist-info");
        let mut record = String::new();
        for (rel, src) in files {
            self.file(&format!("{site}/{rel}"), src);
            record.push_str(&format!("{rel},,\n"));
        }
        for rel in extra {
            self.file(&format!("{site}/{rel}"), "\0binary");
            record.push_str(&format!("{rel},,\n"));
        }
        record.push_str(&format!("{name}-{version}.dist-info/METADATA,,\n{name}-{version}.dist-info/RECORD,,\n"));
        self.file(&format!("{info}/METADATA"), &format!("Metadata-Version: 2.1\nName: {name}\nVersion: {version}\n"))
            .file(&format!("{info}/top_level.txt"), &format!("{name}\n"))
            .file(&format!("{info}/RECORD"), &record)
    }

    fn run(&self, args: &[&str]) -> Run {
        let out = Command::new(env!("CARGO_BIN_EXE_pymapper"))
            .arg("deps")
            .args(args)
            .current_dir(&self.dir)
            .env_remove("VIRTUAL_ENV")
            .output()
            .expect("run pymapper");
        Run {
            code: out.status.code().expect("exited normally"),
            stdout: String::from_utf8(out.stdout).expect("utf-8 stdout"),
            stderr: String::from_utf8(out.stderr).expect("utf-8 stderr"),
        }
    }
}

impl Drop for Project {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

struct Run {
    code: i32,
    stdout: String,
    stderr: String,
}

impl Run {
    fn expect_code(&self, code: i32) -> &Run {
        assert_eq!(self.code, code, "stdout:\n{}\nstderr:\n{}", self.stdout, self.stderr);
        self
    }

    /// The table row of distribution `name` (split on whitespace).
    fn row(&self, name: &str) -> Option<Vec<&str>> {
        self.stdout.lines().find(|l| l.split_whitespace().next() == Some(name)).map(|l| l.split_whitespace().collect())
    }
}

/// Value of a `--stats` counter.
fn stat(stderr: &str, key: &str) -> u64 {
    let line = stderr
        .lines()
        .find(|l| l.split_whitespace().next() == Some(key) && !l.ends_with(" ms"))
        .unwrap_or_else(|| panic!("no {key} in:\n{stderr}"));
    line.split_whitespace().nth(1).expect("value").parse().expect("number")
}

const FOO: &str = "\
def bar():
    return _helper()


def _helper():
    return 1


def baz(a, b, c, d, e, f, g):
    if a:
        return b
    return c
";

/// `app` uses `foo` (calls `foo.bar`) and imports `fastlib` (compiled: no Python code);
/// `idle` is installed but unused.
fn project(name: &str) -> Project {
    let p = Project::new(name);
    p.file("pyproject.toml", "[project]\nname = \"app\"\nversion = \"0.1\"\n")
        .file("app/__init__.py", "")
        .file(
            "app/main.py",
            "import fastlib\nimport foo\n\n\ndef main():\n    return foo.bar() + fastlib.x\n\n\nif __name__ == \"__main__\":\n    main()\n",
        )
        .file("app/other.py", "def other():\n    return 2\n")
        .file(".venv/pyvenv.cfg", "home = /nonexistent/bin\nversion = 3.12.4\n")
        .dist("foo", "1.0", &[("foo/__init__.py", FOO)], &[])
        .dist("idle", "2.0", &[("idle/__init__.py", "def f():\n    return 0\n")], &[])
        .dist("fastlib", "3.0", &[], &["fastlib.cpython-312-darwin.so"]);
    p
}

#[test]
fn lists_used_distributions_with_usage() {
    let p = project("usage");
    let r = p.run(&["--no-cache"]);
    r.expect_code(0);
    assert!(r.stdout.starts_with("distribution  version  importers  api used  loc reached\n"), "{}", r.stdout);
    // foo: one importer, `bar` used; reached: bar, _helper and the module lines outside defs.
    assert_eq!(r.row("foo").expect("foo row"), ["foo", "1.0", "1", "1", "8/12", "(67%)"], "{}", r.stdout);
    assert_eq!(r.row("fastlib").expect("fastlib row"), ["fastlib", "3.0", "1", "-", "-", "(code", "not", "loaded)"]);
    assert!(r.row("idle").is_none(), "{}", r.stdout);
    assert!(
        r.stdout.ends_with("2 of 3 distributions used by first-party code (3 first-party files); --all lists every one.\n"),
        "{}",
        r.stdout
    );
    assert!(r.stderr.is_empty(), "{}", r.stderr);
    let all = p.run(&["--no-cache", "--all"]);
    all.expect_code(0);
    assert_eq!(all.row("idle").expect("idle row"), ["idle", "2.0", "0", "0", "0/2", "(0%)"]);
}

#[test]
fn scorecards_are_cached_and_never_change_the_exit_code() {
    let p = project("score");
    let cold = p.run(&["--score", "--stats"]);
    cold.expect_code(0);
    let foo = cold.row("foo").expect("foo row");
    // grade, score, loc 12, hot/kloc (baz has 7 parameters: one hotspot in 12 lines), mi, cc,
    // no external dependencies, then the usage columns with the public API (bar, baz).
    assert_eq!(foo[4], "12", "{}", cold.stdout);
    assert_eq!(foo[5], "83.3", "{}", cold.stdout);
    assert_eq!(&foo[8..], ["0", "1", "1/2", "8/12", "(67%)"], "{}", cold.stdout);
    assert!(cold.stdout.contains("Scorecards: 2 computed, 0 cached."), "{}", cold.stdout);
    assert_eq!(stat(&cold.stderr, "scorecards_computed"), 2);
    assert!(p.dir.join(".pymapper_cache/scores.pmc").is_file());
    let warm = p.run(&["--score", "--stats"]);
    warm.expect_code(0);
    assert_eq!(stat(&warm.stderr, "scorecards_cached"), 2, "{}", warm.stderr);
    assert_eq!(stat(&warm.stderr, "scorecards_computed"), 0);
    let table = |s: &str| s.lines().take_while(|l| !l.is_empty()).map(str::to_owned).collect::<Vec<_>>();
    assert_eq!(table(&cold.stdout), table(&warm.stdout));
    assert!(warm.stdout.contains("Scorecards: 0 computed, 2 cached."), "{}", warm.stdout);
}

#[test]
fn json_output() {
    let p = project("json");
    let r = p.run(&["--no-cache", "--score", "--format", "json"]);
    r.expect_code(0);
    let lines: Vec<&str> = r.stdout.lines().collect();
    assert!(lines[0].starts_with("{\"version\":2,\"tool\":{\"name\":\"pymapper\",\"version\":\""), "{}", r.stdout);
    assert!(lines[0].ends_with("\"distributions\":["), "{}", r.stdout);
    assert!(lines[1].starts_with("{\"name\":\"fastlib\",\"version\":\"3.0\",\"loaded\":false,\"used\":true,\"importers\":1,"));
    assert!(lines[1].ends_with("\"scorecard\":null},"), "{}", r.stdout);
    assert!(lines[2].starts_with("{\"name\":\"foo\",\"version\":\"1.0\",\"loaded\":true,\"used\":true,\"importers\":1,\"import_rows\":1,"));
    assert!(lines[2].contains("\"api_used\":1,\"call_sites\":1,\"loc_reached\":8,\"loc\":12,\"scorecard\":{\"grade\":\""), "{}", r.stdout);
    assert!(lines[2].contains("\"functions\":3,\"public_api\":2,") && lines[2].contains("\"hotspots\":1,"), "{}", r.stdout);
    assert_eq!(
        lines[3],
        "],\"summary\":{\"distributions\":3,\"used\":2,\"listed\":2,\"first_party_files\":3,\"scorecards_computed\":2,\"scorecards_cached\":0}}"
    );
}

#[test]
fn positional_paths_restrict_the_first_party_code_counted() {
    let p = project("scope");
    let r = p.run(&["--no-cache", "--all", "app/other.py"]);
    r.expect_code(0);
    assert_eq!(r.row("foo").expect("foo row"), ["foo", "1.0", "0", "0", "0/12", "(0%)"], "{}", r.stdout);
    assert!(r.stdout.contains("0 of 2 distributions used by first-party code (1 first-party file)."), "{}", r.stdout);
    // `.` at the project root filters nothing.
    let all = p.run(&["--no-cache", "."]);
    assert_eq!(all.row("foo").expect("foo row")[2], "1");
    let missing = p.run(&["--no-cache", "nope.py"]);
    missing.expect_code(2);
    assert!(missing.stderr.contains("does not exist"), "{}", missing.stderr);
}

#[test]
fn deps_are_always_loaded_and_unsupported_formats_are_usage_errors() {
    let p = project("flags");
    let r = p.run(&["--no-cache", "--no-deps"]);
    r.expect_code(0);
    assert!(r.stderr.contains("--no-deps is ignored"), "{}", r.stderr);
    assert!(r.row("foo").is_some(), "{}", r.stdout);
    for f in ["sarif", "github"] {
        let r = p.run(&["--no-cache", "--format", f]);
        r.expect_code(2);
        assert!(r.stderr.contains("concise and json"), "{}", r.stderr);
        assert!(r.stdout.is_empty());
    }
    let bad = p.run(&["--set", "no-such-option = 1"]);
    assert_eq!(bad.code, 2, "{}", bad.stderr);
}

#[test]
fn no_venv_warns_and_exits_0() {
    let p = Project::new("novenv");
    p.file("pyproject.toml", "[project]\nname = \"app\"\n").file("app/__init__.py", "import requests\n");
    let r = p.run(&["--no-cache"]);
    r.expect_code(0);
    assert!(r.stderr.contains("no virtualenv found"), "{}", r.stderr);
    assert!(r.stdout.contains("0 of 0 distributions"), "{}", r.stdout);
}

/// `--llm` (ISSUES O4): header facts, one line per distribution, most used first, the cap
/// and trailer, and the scorecard line with its grade and public API kept apart from the
/// names used (never a fraction that can exceed 100%).
#[test]
fn llm_listing_and_scorecards() {
    let p = project("llm");
    let r = p.run(&["--no-cache", "--llm"]);
    r.expect_code(0);
    let want = "\
# pymapper deps
root: . · 2 of 3 distributions used by first-party code (3 first-party files) · listed: 2
Usage counts static imports and resolved calls from first-party code; dynamic imports, CLI tools, plugins and unresolved calls are not seen. Scorecards are information, never findings (exit 0).

foo 1.0: importers 1 · names used 1 · loc reached 8/12 (67%)
fastlib 3.0: importers 1 · 1 import row · code not loaded

1 unused distribution not listed (--all lists every one).

Next: `pymapper deps --score --llm` for scorecards · `pymapper deps --check --llm` for undeclared and unused dependencies
";
    assert_eq!(r.stdout, want);
    let r = p.run(&["--no-cache", "--llm", "--limit", "1", "--all"]);
    r.expect_code(0);
    assert!(r.stdout.contains("\nfoo 1.0: importers 1 ·"), "{}", r.stdout);
    assert!(!r.stdout.contains("fastlib"), "{}", r.stdout);
    assert!(r.stdout.contains("\n2 more distributions not displayed. Use --limit 0 to show all.\n"), "{}", r.stdout);
    let r = p.run(&["--no-cache", "--llm", "--score"]);
    r.expect_code(0);
    assert!(r.stdout.starts_with("# pymapper deps --score\n"), "{}", r.stdout);
    let foo = r.stdout.lines().find(|l| l.starts_with("foo 1.0: grade ")).unwrap_or_else(|| panic!("{}", r.stdout));
    assert!(foo.contains(" · importers 1 · names used 1 (public api 2) · loc reached 8/12 (67%)"), "{foo}");
    assert!(r.stdout.contains("\nfastlib 3.0: grade - (code not loaded) · importers 1 ·"), "{}", r.stdout);
}
