//! End-to-end tests of `pymapper check`: the real binary on small generated projects.

use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU32, Ordering};

/// `args`, preceded by `--output-format concise` unless they choose a format: these tests
/// read concise lines, and the default format is `human`.
fn concise<'a>(args: &[&'a str]) -> Vec<&'a str> {
    let chooses = args.iter().any(|a| *a == "--llm" || *a == "--format" || a.starts_with("--output-format") || a.starts_with("--format="));
    let mut v = if chooses { Vec::new() } else { vec!["--output-format", "concise"] };
    v.extend_from_slice(args);
    v
}

/// A throwaway project directory, removed on drop.
struct Project {
    dir: PathBuf,
}

impl Project {
    fn new(name: &str) -> Project {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("pymapper-e2e-{}-{n}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create project dir");
        // Resolve symlinks (macOS /var -> /private/var) so displayed paths are predictable.
        let dir = dir.canonicalize().expect("canonicalize");
        Project { dir }
    }

    /// Writes `text` to `rel` (parents created).
    fn file(&self, rel: &str, text: &str) -> &Project {
        let p = self.dir.join(rel);
        std::fs::create_dir_all(p.parent().expect("parent")).expect("create parents");
        std::fs::write(&p, text).expect("write file");
        self
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.dir.join(rel)
    }

    /// Runs `pymapper check <args>` in the project directory.
    fn check(&self, args: &[&str]) -> Run {
        self.check_in("", args)
    }

    /// Runs `pymapper check <args>` in the directory `rel` of the project.
    fn check_in(&self, rel: &str, args: &[&str]) -> Run {
        self.check_exact(rel, &concise(args))
    }

    /// As [`Project::check_in`], without choosing the concise format.
    fn check_exact(&self, rel: &str, args: &[&str]) -> Run {
        let out = Command::new(env!("CARGO_BIN_EXE_pymapper"))
            .arg("check")
            .args(args)
            .current_dir(self.dir.join(rel))
            .env_remove("VIRTUAL_ENV")
            .output()
            .expect("run pymapper");
        Run::from(out)
    }
}

impl Drop for Project {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Result of one run.
struct Run {
    code: i32,
    stdout: String,
    stderr: String,
}

impl From<Output> for Run {
    fn from(o: Output) -> Run {
        Run {
            code: o.status.code().expect("exited normally"),
            stdout: String::from_utf8(o.stdout).expect("utf-8 stdout"),
            stderr: String::from_utf8(o.stderr).expect("utf-8 stderr"),
        }
    }
}

impl Run {
    /// Finding lines (`path:line:col: CODE message`) of concise output.
    fn findings(&self) -> Vec<&str> {
        self.stdout.lines().filter(|l| l.contains(": PM")).collect()
    }

    /// Rule codes of the findings, in output order.
    fn codes(&self) -> Vec<&str> {
        self.findings()
            .iter()
            .map(|l| {
                let i = l.find(": PM").expect("code") + 2;
                &l[i..i + 5]
            })
            .collect()
    }

    #[track_caller]
    fn expect_code(&self, code: i32) -> &Run {
        assert_eq!(self.code, code, "exit code\nstdout:\n{}\nstderr:\n{}", self.stdout, self.stderr);
        self
    }
}

/// A function with McCabe 12 and cognitive complexity well above 15.
const COMPLEX: &str = "\
def tangled(a, b, c):
    if a:
        if b:
            if c:
                return 1
            elif a > 1:
                return 2
        elif c:
            return 3
    for i in range(a):
        if i % 2:
            while b:
                b -= 1
    try:
        pass
    except ValueError:
        pass
    except KeyError:
        pass
    if a and b or c:
        return 4
    return 0
";

fn simple_project(name: &str) -> Project {
    let p = Project::new(name);
    p.file("pyproject.toml", "[project]\nname = \"app\"\n")
        .file("app/__init__.py", "")
        .file("app/main.py", "from app.util import helper\n\n\ndef run():\n    return helper()\n\n\nif __name__ == '__main__':\n    run()\n")
        .file("app/util.py", "def helper():\n    return 1\n");
    p
}

/// Adds `app/tangled.py` (complex) and makes `app.main` use it, so only complexity rules fire.
fn add_tangled(p: &Project, text: &str) {
    p.file("app/tangled.py", text).file(
        "app/main.py",
        "from app.tangled import tangled\nfrom app.util import helper\n\n\ndef run():\n    return helper() + tangled(1, 2, 3)\n\n\nif __name__ == '__main__':\n    run()\n",
    );
}

#[test]
fn clean_project_exits_zero() {
    let p = simple_project("clean");
    let r = p.check(&[]);
    r.expect_code(0);
    assert!(r.findings().is_empty(), "{}", r.stdout);
    assert!(r.stdout.contains("All checks passed!"), "{}", r.stdout);
}

#[test]
fn complexity_findings_exit_one_with_ruff_style_lines() {
    let p = simple_project("complex");
    add_tangled(&p, COMPLEX);
    let r = p.check(&[]);
    r.expect_code(1);
    let lines = r.findings();
    assert!(lines.iter().any(|l| l.starts_with("app/tangled.py:1:5: PM101 ")), "{}", r.stdout);
    assert!(lines.iter().any(|l| l.starts_with("app/tangled.py:1:5: PM102 ")), "{}", r.stdout);
    assert!(r.stdout.contains("files checked"), "{}", r.stdout);
}

#[test]
fn select_and_ignore() {
    let p = simple_project("select");
    add_tangled(&p, COMPLEX);
    let r = p.check(&["--select", "PM101"]);
    r.expect_code(1);
    assert_eq!(r.codes(), ["PM101"]);
    let r = p.check(&["--ignore", "PM1"]);
    r.expect_code(0);
    assert!(r.codes().is_empty(), "{}", r.stdout);
    // Rules ignored in the config file, re-enabled by a more specific selector.
    p.file("pymapper.toml", "[rules]\nignore = [\"PM1\"]\n");
    p.check(&[]).expect_code(0);
    let r = p.check(&["--extend-select", "PM102"]);
    assert_eq!(r.expect_code(1).codes(), ["PM102"]);
}

#[test]
fn inline_suppressions_and_per_file_ignores() {
    let p = simple_project("suppress");
    let suppressed = COMPLEX.replacen("def tangled(a, b, c):", "def tangled(a, b, c):  # pymapper: ignore[PM101, PM102]", 1);
    add_tangled(&p, &suppressed);
    let r = p.check(&["--select", "PM101,PM102"]);
    r.expect_code(0);
    assert!(r.stdout.contains("suppressed") || r.codes().is_empty(), "{}", r.stdout);

    add_tangled(&p, COMPLEX);
    p.file("tests/test_x.py", &COMPLEX.replace("tangled", "test_tangled"));
    // Test code is out of the metric rules by default.
    let r = p.check(&["--select", "PM101"]);
    assert_eq!(r.expect_code(1).codes(), ["PM101"]);
    let r = p.check(&["--select", "PM101", "--set", "analysis.include-tests-in-metrics = true"]);
    assert_eq!(r.expect_code(1).codes(), ["PM101", "PM101"]);
    p.file("pymapper.toml", "[analysis]\ninclude-tests-in-metrics = true\n[rules.per-file-ignores]\n\"tests/\" = [\"PM1\"]\n");
    let r = p.check(&["--select", "PM101"]);
    let lines = r.expect_code(1).findings();
    assert_eq!(lines.len(), 1, "{}", r.stdout);
    assert!(lines[0].starts_with("app/tangled.py:"), "{}", r.stdout);
}

/// A project with an import cycle `app.a -> app.b -> app.a`.
fn cycle_project(name: &str) -> Project {
    let p = simple_project(name);
    p.file("app/a.py", "from app import b\n\n\ndef fa():\n    return b.fb()\n")
        .file("app/b.py", "from app import a\n\n\ndef fb():\n    return a.fa\n")
        .file("app/main.py", "from app.a import fa\n\n\nif __name__ == '__main__':\n    fa()\n");
    p
}

#[test]
fn import_cycle_in_every_format() {
    let p = cycle_project("formats");
    let r = p.check(&["--select", "PM301"]);
    assert_eq!(r.expect_code(1).codes(), ["PM301"]);
    let first = r.findings()[0];
    assert!(first.starts_with("app/a.py:1:") || first.starts_with("app/b.py:1:"), "{}", r.stdout);
    assert!(first.contains("import cycle (2 modules): "), "{}", r.stdout);

    let r = p.check(&["--select", "PM301", "--format", "json"]);
    r.expect_code(1);
    assert!(r.stdout.starts_with("{\"version\":2,"), "{}", r.stdout);
    assert!(r.stdout.contains("\"code\":\"PM301\""), "{}", r.stdout);
    assert!(r.stdout.contains("\"path\":\"app/"), "{}", r.stdout);
    assert!(r.stdout.contains("\"cycle\":{\"size\":2,\"members\":[\"app.a\",\"app.b\"],\"witness\":["), "{}", r.stdout);

    let r = p.check(&["--select", "PM301", "--output-format", "sarif"]);
    r.expect_code(1);
    assert!(r.stdout.contains("\"version\":\"2.1.0\"") || r.stdout.contains("\"version\": \"2.1.0\""), "{}", r.stdout);
    assert!(r.stdout.contains("PM301"), "{}", r.stdout);

    let r = p.check(&["--select", "PM301", "--format", "github"]);
    r.expect_code(1);
    assert!(r.stdout.lines().any(|l| l.starts_with("::") && l.contains("file=app/")), "{}", r.stdout);

    // The format can come from the config file too.
    p.file("pymapper.toml", "output-format = \"json\"\n");
    let r = p.check_exact("", &["--select", "PM301"]);
    assert!(r.expect_code(1).stdout.starts_with('{'), "{}", r.stdout);
}

#[test]
fn baselines_report_only_new_findings() {
    let p = cycle_project("baseline");
    let r = p.check(&["--save-baseline", "baseline.json"]);
    r.expect_code(0);
    assert!(r.stdout.is_empty(), "{}", r.stdout);
    assert!(r.stderr.contains("saved"), "{}", r.stderr);
    let text = std::fs::read_to_string(p.path("baseline.json")).expect("baseline written");
    assert!(text.contains("\"PM301\""), "{text}");
    let leftovers = std::fs::read_dir(&p.dir).expect("list project").flatten();
    let tmp: Vec<_> = leftovers.filter(|e| e.file_name().to_string_lossy().ends_with(".tmp")).collect();
    assert!(tmp.is_empty(), "temporary files left: {tmp:?}");

    p.check(&["--baseline", "baseline.json"]).expect_code(0);
    // From the config file, relative to it.
    p.file("pymapper.toml", "baseline = \"baseline.json\"\n");
    p.check(&[]).expect_code(0);

    // A new finding is reported; the baselined one is not.
    add_tangled(&p, COMPLEX);
    p.file("app/main.py", "from app.a import fa\nfrom app.tangled import tangled\n\n\nif __name__ == '__main__':\n    fa()\n    tangled(1, 2, 3)\n");
    let r = p.check(&[]);
    r.expect_code(1);
    assert!(!r.codes().contains(&"PM301"), "{}", r.stdout);
    assert!(r.codes().contains(&"PM101"), "{}", r.stdout);

    // A missing or corrupt baseline is a fatal error.
    std::fs::write(p.path("baseline.json"), "{nope").expect("write");
    let r = p.check(&[]);
    r.expect_code(2);
    assert!(r.stderr.contains("invalid baseline"), "{}", r.stderr);
    let r = p.check(&["--baseline", "missing.json"]);
    r.expect_code(2);
    assert!(r.stderr.contains("cannot read baseline"), "{}", r.stderr);
}

#[test]
fn usage_and_config_errors_exit_two() {
    let p = simple_project("errors");
    let r = p.check(&["--no-such-flag"]);
    r.expect_code(2);
    let r = p.check(&["--select", "XX1"]);
    r.expect_code(2);
    let r = p.check(&["--confidence", "101"]);
    r.expect_code(2);
    p.file("pymapper.toml", "selcet = [\"PM1\"]\n");
    let r = p.check(&[]);
    r.expect_code(2);
    assert!(r.stderr.contains("pymapper.toml:1:1"), "{}", r.stderr);
    assert!(r.stderr.contains("unknown option `selcet`"), "{}", r.stderr);
    p.file("pymapper.toml", "[rules.per-file-ignores]\n\"[z-a\" = [\"PM1\"]\n");
    let r = p.check(&[]);
    r.expect_code(2);
    assert!(r.stderr.contains("per-file-ignores"), "{}", r.stderr);
}

#[test]
fn missing_paths_exit_two() {
    let p = simple_project("missing");
    // A mistyped explicit path is a usage error, even next to a valid one.
    for args in [&["missing_dir"][..], &["app", "missing_dir"], &["app/main.py/x"]] {
        let r = p.check(args);
        r.expect_code(2);
        assert!(r.stderr.contains("pymapper: error: path `"), "{args:?}: {}", r.stderr);
        assert!(r.stderr.contains("does not exist"), "{args:?}: {}", r.stderr);
        assert!(!r.stdout.contains("All checks passed"), "{args:?}: {}", r.stdout);
    }
    let r = p.check(&["missing_dir"]);
    assert!(r.stderr.contains(&format!("`{}`", p.path("missing_dir").display())), "{}", r.stderr);
    // A stale configured path too, naming the config file.
    p.file("pymapper.toml", "paths = [\"app\", \"gone\"]\n");
    let r = p.check(&[]);
    r.expect_code(2);
    assert!(r.stderr.contains("(from `paths` in "), "{}", r.stderr);
    assert!(r.stderr.contains("pymapper.toml"), "{}", r.stderr);
    // Positional paths only filter the report: the configured paths are still loaded, so a
    // stale one fails even when the positional path is fine.
    p.check(&["app"]).expect_code(2);
    p.file("pymapper.toml", "paths = [\"app\"]\n");
    p.check(&["app"]).expect_code(0);
}

#[test]
fn unknown_selectors_are_usage_errors() {
    let p = simple_project("unknown-sel");
    let r = p.check(&["--select", "PM999,PM1"]);
    r.expect_code(2);
    assert!(r.stderr.contains("unknown rule selector `PM999`"), "{}", r.stderr);
    assert!(!r.stdout.contains("All checks passed"), "{}", r.stdout);
    let r = p.check(&["--select", "PM1", "--ignore", "PM9"]);
    r.expect_code(2);
    assert!(r.stderr.contains("unknown rule selector `PM9`"), "{}", r.stderr);
    // A near miss gets a suggestion, from the CLI and from the config file alike.
    let r = p.check(&["--select", "PM409"]);
    r.expect_code(2);
    assert!(r.stderr.contains("did you mean `PM401`?"), "{}", r.stderr);
    p.file("pymapper.toml", "[rules]\nextend-select = [\"PM108\"]\n");
    let r = p.check(&[]);
    r.expect_code(2);
    assert!(r.stderr.contains("unknown rule selector `PM108`") && r.stderr.contains("did you mean `PM10"), "{}", r.stderr);
    p.file("pymapper.toml", "[rules.per-file-ignores]\n\"tests/\" = [\"PM77\"]\n");
    p.check(&[]).expect_code(2);
    std::fs::remove_file(p.path("pymapper.toml")).expect("remove config");
    let r = p.check(&["--select", "PM1"]);
    r.expect_code(0);
    assert!(!r.stderr.contains("unknown rule selector"), "{}", r.stderr);
}

/// Selections whose rules cannot run in `check` fail or say so; never a silent "All checks
/// passed!".
#[test]
fn selections_that_cannot_run() {
    let p = simple_project("cannot-run");
    for (sel, needle) in [
        ("PM6", "PM601-PM604: dependency rules are run by `pymapper deps --check`, not `check`"),
        ("PM601", "PM601: dependency rules"),
        ("PM501", "PM501: expensive, needs `analysis.dupes = true`"),
        ("PM302", "PM302: no [[contracts]] are configured"),
        ("PM5,PM6", "PM501: expensive"),
    ] {
        let r = p.check(&["--select", sel]);
        r.expect_code(2);
        assert!(r.stderr.contains("none of the selected rules can run in `check`"), "{sel}: {}", r.stderr);
        assert!(r.stderr.contains(needle), "{sel}: {}", r.stderr);
        assert!(r.stdout.is_empty(), "{sel}: {}", r.stdout);
    }
    let r = p.check(&["--select", "PM1", "--ignore", "PM1"]);
    r.expect_code(2);
    assert!(r.stderr.contains("no rule is selected"), "{}", r.stderr);

    // Some rules run: a warning naming the skipped ones, and the footer counts them.
    let r = p.check(&["--select", "PM1,PM501"]);
    r.expect_code(0);
    assert!(r.stderr.contains("PM501 is selected but does not run") && r.stderr.contains("analysis.dupes"), "{}", r.stderr);
    assert!(r.stdout.contains("7 rules ran, 1 skipped (PM501: needs `analysis.dupes = true`)."), "{}", r.stdout);
    // `ALL` stays quiet on stderr but the footer lists every skipped rule.
    let r = p.check(&["--select", "ALL"]);
    r.expect_code(0);
    assert!(!r.stderr.contains("does not run"), "{}", r.stderr);
    for needle in ["8 skipped", "PM302-PM304: no [[contracts]]", "PM501: needs `analysis.dupes = true`", "PM601-PM604: run by `pymapper deps --check`"] {
        assert!(r.stdout.contains(needle), "{needle}: {}", r.stdout);
    }
    // `PM3` without contracts is a normal way to ask for cycles: no warning.
    let r = p.check(&["--select", "PM3"]);
    r.expect_code(0);
    assert!(!r.stderr.contains("does not run"), "{}", r.stderr);
    assert!(r.stdout.contains("1 rule ran, 3 skipped"), "{}", r.stdout);
    // The default selection prints no rule counts (skipping gated rules is the default).
    let r = p.check(&[]);
    assert!(!r.stdout.contains("skipped"), "{}", r.stdout);
    // `--stats` lists the counts and reasons.
    let r = p.check(&["--select", "ALL", "--stats"]);
    assert!(r.stderr.contains("rules_run") && r.stderr.contains("skipped PM501: expensive"), "{}", r.stderr);
}

#[test]
fn stats_go_to_stderr() {
    let p = cycle_project("stats");
    let plain = p.check(&["--no-cache"]);
    let r = p.check(&["--no-cache", "--stats"]);
    r.expect_code(1);
    assert_eq!(r.stdout, plain.stdout, "--stats must not change stdout");
    for key in ["pymapper: stats", "roots", "files", "load.extract", "resolve.imports", "cycles", "total", "peak_rss"] {
        assert!(r.stderr.contains(key), "missing {key}:\n{}", r.stderr);
    }
    assert!(!r.stderr.contains("cache_hits"), "{}", r.stderr);
}

#[test]
fn cache_is_written_reused_and_optional() {
    let p = cycle_project("cache");
    add_tangled(&p, COMPLEX);
    let cold = p.check(&["--stats"]);
    cold.expect_code(1);
    assert!(p.path(".pymapper_cache").is_dir());
    assert!(cold.stderr.contains("cache_misses"), "{}", cold.stderr);
    let warm = p.check(&["--stats"]);
    assert_eq!(warm.stdout, cold.stdout);
    let hits = |s: &str| -> u64 {
        let line = s.lines().find(|l| l.trim_start().starts_with("cache_hits")).expect("cache_hits");
        line.split_whitespace().nth(1).expect("value").parse().expect("number")
    };
    assert_eq!(hits(&cold.stderr), 0);
    assert!(hits(&warm.stderr) >= 6, "{}", warm.stderr);
    // An edit is picked up.
    p.file("app/tangled.py", "def tangled(a, b, c):\n    return 0\n");
    let edited = p.check(&[]);
    assert!(!edited.codes().contains(&"PM101"), "{}", edited.stdout);

    let q = simple_project("nocache");
    q.check(&["--no-cache"]).expect_code(0);
    assert!(!q.path(".pymapper_cache").exists());
    q.check(&["--cache-dir", "elsewhere"]).expect_code(0);
    assert!(q.path("elsewhere").is_dir());
}

#[test]
fn confidence_gate_and_threads() {
    let p = simple_project("confidence");
    // A nested function returned by its parent looks used only through the return value.
    p.file("app/util.py", "def helper():\n    def inner():\n        return 1\n    return inner\n");
    let default = p.check(&[]);
    let all = p.check(&["--confidence", "0"]);
    assert!(all.findings().len() > default.findings().len(), "{}\n---\n{}", all.stdout, default.stdout);
    assert!(default.stdout.contains("below min-confidence"), "{}", default.stdout);
    let one = p.check(&["--confidence", "0", "--threads", "1"]);
    assert_eq!(one.stdout, all.stdout);
    let j = p.check(&["--confidence", "0", "-j", "3", "--no-cache"]);
    assert_eq!(j.stdout, all.stdout);
}

#[test]
fn layering_contracts_from_config() {
    let p = simple_project("layers");
    p.file("app/ui.py", "from app import db\n\n\ndef show():\n    return db.get()\n")
        .file("app/db.py", "from app import ui\n\n\ndef get():\n    return ui\n")
        .file(
            "pymapper.toml",
            "[[contracts]]\nname = \"ui above db\"\nkind = \"layers\"\nlayers = [\"app.ui\", \"app.db\"]\n",
        );
    let r = p.check(&["--select", "PM302"]);
    let lines = r.expect_code(1).findings();
    assert_eq!(lines.len(), 1, "{}", r.stdout);
    assert!(lines[0].starts_with("app/db.py:1:"), "{}", r.stdout);
    assert!(lines[0].contains("ui above db"), "{}", r.stdout);
    // A contract naming an unknown module warns.
    p.file(
        "pymapper.toml",
        "[[contracts]]\nname = \"typo\"\nkind = \"layers\"\nlayers = [\"app.ui\", \"app.dbb\"]\n",
    );
    let r = p.check(&["--select", "PM302"]);
    assert!(r.stderr.contains("app.dbb"), "{}", r.stderr);
}

#[test]
fn closed_stdout_is_not_an_error() {
    let p = Project::new("epipe");
    let many: String = (0..3000).map(|i| format!("from os import path as p{i}\n")).collect();
    p.file("app/__init__.py", "from app import many\n").file("app/many.py", &many);
    let mut child = Command::new(env!("CARGO_BIN_EXE_pymapper"))
        .args(["check", "--no-cache", "--output-format", "concise"])
        .current_dir(&p.dir)
        .env_remove("VIRTUAL_ENV")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn");
    let mut out = child.stdout.take().expect("stdout");
    let mut first = [0u8; 16];
    std::io::Read::read_exact(&mut out, &mut first).expect("some output");
    drop(out);
    let o = child.wait_with_output().expect("wait");
    let stderr = String::from_utf8_lossy(&o.stderr);
    assert_eq!(o.status.code(), Some(1), "{stderr}");
    assert!(!stderr.contains("error"), "{stderr}");
    assert!(!stderr.contains("panicked"), "{stderr}");
}

#[test]
fn no_files_warns() {
    let p = Project::new("empty");
    p.file("pyproject.toml", "[project]\nname = \"empty\"\n").file("notes.txt", "x");
    let r = p.check(&[]);
    r.expect_code(0);
    assert!(r.stderr.contains("no first-party Python files"), "{}", r.stderr);
}

/// Value of a `--stats` counter.
fn stat(stderr: &str, key: &str) -> u64 {
    let line = stderr
        .lines()
        .find(|l| l.split_whitespace().next() == Some(key) && !l.ends_with(" ms"))
        .unwrap_or_else(|| panic!("no {key} in:\n{stderr}"));
    line.split_whitespace().nth(1).expect("value").parse().expect("number")
}

#[test]
fn third_party_code_is_loaded_with_deps_but_never_reported() {
    let p = simple_project("deps");
    p.file("app/util.py", "import foo\n\n\ndef helper():\n    return foo.bar()\n")
        .file(".venv/pyvenv.cfg", "home = /nonexistent/bin\nversion = 3.12.4\n")
        .file(".venv/lib/python3.12/site-packages/foo/__init__.py", COMPLEX.replace("tangled", "bar").as_str())
        .file(".venv/lib/python3.12/site-packages/foo-1.0.dist-info/METADATA", "Metadata-Version: 2.1\nName: foo\nVersion: 1.0\n")
        .file(".venv/lib/python3.12/site-packages/foo-1.0.dist-info/top_level.txt", "foo\n")
        .file(
            ".venv/lib/python3.12/site-packages/foo-1.0.dist-info/RECORD",
            "foo/__init__.py,,\nfoo-1.0.dist-info/METADATA,,\nfoo-1.0.dist-info/RECORD,,\n",
        );
    let without = p.check(&["--no-cache", "--stats"]);
    without.expect_code(0);
    assert_eq!(stat(&without.stderr, "files"), stat(&without.stderr, "first_party_files"));
    let with = p.check(&["--no-cache", "--stats", "--deps"]);
    with.expect_code(0);
    assert_eq!(with.stdout, without.stdout);
    assert_eq!(stat(&with.stderr, "files"), stat(&with.stderr, "first_party_files") + 1, "{}", with.stderr);
    // Through the cache: the same result, cold and warm.
    let cold = p.check(&["--deps"]);
    let warm = p.check(&["--deps", "--stats"]);
    assert_eq!(cold.stdout, without.stdout);
    assert_eq!(warm.stdout, without.stdout);
    assert!(stat(&warm.stderr, "cache_hits") >= 1, "{}", warm.stderr);
    // The stdlib is found through PATH when an interpreter is installed; when it is not found
    // that is a warning, not an error. Either way first-party results do not change.
    let r = p.check(&["--no-cache", "--stdlib", "--deps", "--stats"]);
    r.expect_code(0);
    assert_eq!(r.stdout, without.stdout);
    let extra = stat(&r.stderr, "files") - stat(&r.stderr, "first_party_files");
    assert!(extra > 1 || r.stderr.contains("standard library"), "{}", r.stderr);
}

/// A varied (non-periodic) function body, long enough to be a clone with default thresholds.
const DUPLICATED_BODY: &str = "\
    result = []
    seen = {}
    for index, item in enumerate(value):
        if item in seen:
            seen[item] += 1
            continue
        seen[item] = 1
        key = (item, index % 7, offset)
        try:
            weight = float(item) / (index + 1)
        except (TypeError, ValueError):
            weight = 0.0
        while weight > 10:
            weight = weight / 2 - offset
        result.append({'key': key, 'weight': weight, 'name': str(item).upper()})
    result.sort(key=lambda entry: (entry['weight'], entry['name']))
    total = sum(entry['weight'] for entry in result if entry['weight'] > 0)
    return result, total, len(seen)
";

#[test]
fn duplication_is_gated() {
    let p = simple_project("dupes");
    let f = |name: &str| format!("def {name}(value, offset):\n{DUPLICATED_BODY}");
    p.file("app/one.py", &f("first")).file("app/two.py", &f("second")).file(
        "app/main.py",
        "from app.one import first\nfrom app.two import second\n\n\nif __name__ == '__main__':\n    first(1, 2)\n    second(1, 2)\n",
    );
    let r = p.check(&["--select", "PM501"]);
    r.expect_code(2);
    assert!(r.stderr.contains("PM501") && r.stderr.contains("analysis.dupes"), "{}", r.stderr);
    let r = p.check(&["--select", "PM501", "--set", "analysis.dupes = true"]);
    r.expect_code(1);
    assert!(!r.codes().is_empty() && r.codes().iter().all(|c| *c == "PM501"), "{}", r.stdout);
    // Warm cache with fingerprints gives the same findings.
    let again = p.check(&["--select", "PM501", "--set", "analysis.dupes = true"]);
    assert_eq!(again.stdout, r.stdout);
}

#[test]
fn project_scripts_are_entrypoints() {
    let p = simple_project("scripts");
    p.file("app/cli.py", "def main():\n    return 0\n");
    let r = p.check(&["--select", "PM401", "--confidence", "0"]);
    assert!(r.stdout.contains("app.cli:main"), "{}", r.stdout);
    p.file("pyproject.toml", "[project]\nname = \"app\"\n\n[project.scripts]\napp = \"app.cli:main\"\n");
    let r = p.check(&["--select", "PM401", "--confidence", "0"]);
    assert!(!r.stdout.contains("app.cli:main"), "{}", r.stdout);
}

/// A monorepo of two units: `apps/app` declares a script (application mode), `libs/lib` does
/// not (library mode). The root unit has no Python files.
fn monorepo(name: &str) -> Project {
    let p = Project::new(name);
    p.file("pyproject.toml", "[project]\nname = \"mono\"\n\n[tool.pymapper]\nsrc = [\"apps/app\", \"libs/lib\"]\n")
        .file("apps/app/pyproject.toml", "[project]\nname = \"app\"\n\n[project.scripts]\napp = \"appkg.cli:main\"\n")
        .file("apps/app/appkg/__init__.py", "")
        .file("apps/app/appkg/cli.py", "from appkg.helpers import used\n\n\ndef main():\n    return used()\n")
        .file("apps/app/appkg/helpers.py", "def used():\n    return 1\n\n\ndef orphan():\n    return 2\n")
        .file("libs/lib/pyproject.toml", "[project]\nname = \"lib\"\n")
        .file("libs/lib/libkg/__init__.py", "")
        .file("libs/lib/libkg/api.py", "def public_tool():\n    return 1\n\n\ndef _private_unused():\n    return 2\n");
    p
}

#[test]
fn auto_mode_is_decided_per_project_unit() {
    let p = monorepo("units");
    let r = p.check(&["--no-cache", "--stats", "--select", "PM4"]);
    r.expect_code(1);
    let f = r.findings().join("\n");
    // Application unit: what the script does not reach is dead.
    assert!(f.contains("appkg.helpers:orphan"), "{f}");
    // Library unit: public names are API; private unused ones are still dead.
    assert!(!f.contains("public_tool"), "{f}");
    assert!(f.contains("libkg.api:_private_unused"), "{f}");
    assert_eq!(stat(&r.stderr, "deadcode.units"), 2, "{}", r.stderr);
    assert_eq!(stat(&r.stderr, "deadcode.library"), 1);
    assert_eq!(stat(&r.stderr, "deadcode.application"), 1);
    assert!(!r.stderr.contains("no entry points"), "{}", r.stderr);
    // Forcing application mode reports the library's public function too (PM404 off, so the
    // module finding does not contain it), and warns about the unit that has no entry point
    // (only that one).
    let r = p.check(&["--no-cache", "--stats", "--select", "PM401", "--mode", "application"]);
    let f = r.findings().join("\n");
    assert!(f.contains("libkg.api:public_tool") && f.contains("appkg.helpers:orphan"), "{f}");
    assert_eq!(stat(&r.stderr, "deadcode.application"), 2);
    let warnings: Vec<&str> = r.stderr.lines().filter(|l| l.contains("no entry points found")).collect();
    assert_eq!(warnings.len(), 1, "{}", r.stderr);
    assert!(warnings[0].ends_with("every definition will look unused. Consider --mode library or [entrypoints]"), "{}", warnings[0]);
    assert!(warnings[0].contains(&format!("in {} (", p.path("libs/lib").display())), "{}", warnings[0]);
    // Forcing library mode keeps the app's public helper.
    let r = p.check(&["--no-cache", "--select", "PM4", "--mode", "library"]);
    assert!(!r.stdout.contains("orphan") && !r.stdout.contains("public_tool"), "{}", r.stdout);
}

#[test]
fn single_package_mode_is_unchanged() {
    // Application (a `__main__` guard): the unreachable public function is reported.
    let p = simple_project("single-app");
    p.file("app/extra.py", "def lonely():\n    return 1\n");
    let r = p.check(&["--no-cache", "--stats", "--select", "PM401", "--confidence", "0"]);
    assert!(r.stdout.contains("app.extra:lonely"), "{}", r.stdout);
    assert_eq!((stat(&r.stderr, "deadcode.units"), stat(&r.stderr, "deadcode.application")), (1, 1));
    let forced = p.check(&["--no-cache", "--select", "PM401", "--confidence", "0", "--mode", "application"]);
    assert_eq!(forced.stdout, r.stdout);
    // Library (no guard, no scripts): public functions are API, at any confidence.
    let p = Project::new("single-lib");
    p.file("pyproject.toml", "[project]\nname = \"lib\"\n")
        .file("lib/__init__.py", "")
        .file("lib/api.py", "def tool():\n    return 1\n");
    let r = p.check(&["--no-cache", "--stats", "--select", "PM4"]);
    r.expect_code(0);
    assert_eq!((stat(&r.stderr, "deadcode.units"), stat(&r.stderr, "deadcode.library")), (1, 1));
    let forced = p.check(&["--no-cache", "--select", "PM4", "--mode", "library"]);
    assert_eq!(forced.stdout, r.stdout);
}

#[test]
fn entrypoint_names_cover_a_whole_module() {
    // Functions a framework loads by name from YAML: `module:*` keeps them all.
    let p = monorepo("ep-names");
    let args = ["--no-cache", "--select", "PM401", "--mode", "application"];
    let r = p.check(&args);
    assert!(r.stdout.contains("libkg.api:public_tool"), "{}", r.stdout);
    let r = p.check(&[&args[..], &["--set", "entrypoints.names = [\"libkg.api:*\"]"]].concat());
    assert!(!r.stdout.contains("libkg.api:"), "{}", r.stdout);
    assert!(r.stdout.contains("appkg.helpers:orphan"), "{}", r.stdout);
}

#[test]
fn application_mode_without_entry_points_warns_and_words_findings() {
    let p = Project::new("no-entry");
    p.file("pyproject.toml", "[project]\nname = \"pkg\"\n")
        .file("pkg/__init__.py", "")
        .file("pkg/a.py", "from pkg import b\n\n\ndef f():\n    return b.g()\n")
        .file("pkg/b.py", "def g():\n    return 1\n")
        .file("pkg/c.py", "def h():\n    return 2\n");
    // `b` occurs in `a` (name evidence): confidence 50, shown with `--confidence 0`.
    let r = p.check(&["--no-cache", "--select", "PM404", "--mode", "application", "--confidence", "0"]);
    r.expect_code(1);
    let f = r.findings().join("\n");
    assert!(f.contains("module `pkg.a` is never imported"), "{f}");
    assert!(f.contains("module `pkg.b` is only imported by unused modules"), "{f}");
    assert!(f.contains("module `pkg.c` is never imported"), "{f}");
    // One warning, for the project (the mode is forced and no unit has an entry point).
    let warnings: Vec<&str> = r.stderr.lines().filter(|l| l.contains("no entry points found")).collect();
    assert_eq!(warnings, [format!(
        "pymapper: warning: no entry points found in {} (no __main__ guard, [project.scripts], entrypoint rules or tests); \
         every definition will look unused. Consider --mode library or [entrypoints]",
        p.path("").display().to_string().trim_end_matches('/')
    )]);
    let r = p.check(&["--no-cache", "--select", "PM401", "--mode", "application", "--confidence", "0"]);
    let f = r.findings().join("\n");
    assert!(f.contains("function `pkg.a:f` is never used"), "{f}");
    assert!(f.contains("function `pkg.b:g` is only used by unused code"), "{f}");
    assert!(f.contains("function `pkg.c:h` is never used"), "{f}");
    // An entry point silences the warning; what it reaches is live, the rest keeps its wording.
    p.file("pkg/a.py", "from pkg import b\n\n\ndef f():\n    return b.g()\n\n\nif __name__ == '__main__':\n    f()\n");
    let r = p.check(&["--no-cache", "--select", "PM401,PM404", "--mode", "application"]);
    assert!(!r.stderr.contains("no entry points"), "{}", r.stderr);
    assert_eq!(r.findings().len(), 1, "{}", r.stdout);
    assert!(r.stdout.contains("module `pkg.c` is never imported"), "{}", r.stdout);
    // Library mode: no warning either.
    let r = p.check(&["--no-cache", "--mode", "library"]);
    assert!(!r.stderr.contains("no entry points"), "{}", r.stderr);
}

// ----- positional paths filter a whole-program run -----

/// A uv workspace whose members share the PEP 420 namespace package `ns`, plus a loose script
/// (in `cli/`: a `__main__` guard under a helper directory such as `tools/` is no entry point).
/// No venv: the members' import roots come from their `pyproject.toml` files.
fn namespace_workspace(name: &str) -> Project {
    let p = Project::new(name);
    p.file("pyproject.toml", "[project]\nname = \"mono\"\n[tool.uv.workspace]\nmembers = [\"libs/*\"]\n")
        .file("libs/core/pyproject.toml", "[project]\nname = \"ns-core\"\n")
        .file("libs/core/ns/core/util.py", "def used():\n    return _helper()\n\n\ndef _helper():\n    return 1\n\n\ndef _unused_private():\n    return 2\n")
        .file("libs/core/ns/core/_orphan.py", "def _orphan():\n    return 1\n")
        .file("libs/app/pyproject.toml", "[project]\nname = \"ns-app\"\n[project.scripts]\napp = \"ns.app.main:main\"\n")
        .file(
            "libs/app/ns/app/main.py",
            "import os\nfrom ns.core.util import used\n\n\ndef main():\n    return used()\n\n\ndef dead_app():\n    return 3\n",
        )
        .file("libs/app/ns/app/stale.py", "def stale():\n    return 1\n")
        .file("cli/run.py", "def tool_dead():\n    pass\n\n\nif __name__ == '__main__':\n    print(1)\n");
    p
}

/// Finding lines of `whole` whose path starts with one of `prefixes`.
fn restricted<'a>(whole: &'a Run, prefixes: &[&str]) -> Vec<&'a str> {
    whole.findings().into_iter().filter(|l| prefixes.iter().any(|p| l.starts_with(p))).collect()
}

/// Finding lines whose paths are relative to the project root, as shown from its
/// subdirectory `dir` (paths are relative to the working directory). Order is kept: callers
/// compare sets of lines from runs in the same directory.
fn rebased(lines: &[&str], dir: &str) -> Vec<String> {
    let depth = dir.split('/').count();
    let mut out: Vec<String> = lines
        .iter()
        .map(|l| match l.strip_prefix(&format!("{dir}/")) {
            Some(rest) => rest.to_owned(),
            None => {
                // Up to the shared directory, then down.
                let base: Vec<&str> = dir.split('/').collect();
                let parts: Vec<&str> = l.split('/').collect();
                let shared = base.iter().zip(&parts).take_while(|(a, b)| a == b).count();
                format!("{}{}", "../".repeat(depth - shared), parts[shared..].join("/"))
            }
        })
        .collect();
    out.sort();
    out
}

fn sorted(lines: Vec<&str>) -> Vec<String> {
    let mut v: Vec<String> = lines.into_iter().map(str::to_owned).collect();
    v.sort();
    v
}

#[test]
fn positional_paths_filter_the_whole_program_findings() {
    let p = namespace_workspace("scope-ns");
    let args = ["--confidence", "0"];
    let whole = p.check(&args);
    whole.expect_code(1);
    // Module names come from the members' import roots: `ns.app.main`, not `app.main`.
    let all = whole.findings().join("\n");
    for needle in ["ns.app.main:dead_app", "ns.core.util:_unused_private", "module `ns.app.stale`", "cli.run:tool_dead"] {
        assert!(all.contains(needle), "{needle}:\n{all}");
    }
    assert!(!all.contains("`app.") && !all.contains("`core."), "{all}");
    for scope in [
        &["libs/app"][..],
        &["libs/app/"],
        &["libs/core/ns/core/util.py"],
        &["libs/app/ns/app/main.py", "cli"],
        &["libs/core", "libs/core/ns"],
        &["libs/app/ns/app/__init__.py/.."],
    ] {
        let r = p.check(&[&args[..], scope].concat());
        let prefixes: Vec<String> = scope.iter().map(|s| s.replace("/__init__.py/..", "").trim_end_matches('/').to_owned()).collect();
        let prefixes: Vec<&str> = prefixes.iter().map(String::as_str).collect();
        assert_eq!(r.findings(), restricted(&whole, &prefixes), "{scope:?}\n{}{}", r.stdout, r.stderr);
        assert!(!r.findings().is_empty(), "{scope:?}");
        assert!(r.stdout.contains("Filtered to "), "{scope:?}: {}", r.stdout);
        assert!(r.stdout.contains("the whole project was analysed"), "{scope:?}: {}", r.stdout);
    }
    // The footer counts the files in scope.
    let r = p.check(&[&args[..], &["libs/core/ns/core/util.py"]].concat());
    assert!(r.stdout.contains("Found 1 finding in 1 file (1 file checked)."), "{}", r.stdout);
    assert!(r.stdout.contains("Filtered to libs/core/ns/core/util.py (1 of 5 first-party files)"), "{}", r.stdout);
    // A file with no findings passes, but only because it is clean.
    p.file("libs/core/ns/core/clean.py", "from ns.core.util import used\n\nVALUE = used()\n");
    let r = p.check(&[&args[..], &["libs/core/ns/core/clean.py"]].concat());
    r.expect_code(0);
    assert!(r.stdout.contains("All checks passed! (1 file checked)"), "{}", r.stdout);
    // From inside a member, with relative paths, warm cache: the same answers (paths shown
    // relative to the working directory).
    let r = p.check_in("libs/app", &[&args[..], &["."]].concat());
    assert_eq!(sorted(r.findings()), rebased(&restricted(&whole, &["libs/app"]), "libs/app"), "{}{}", r.stdout, r.stderr);
    let r = p.check_in("libs/app/ns/app", &[&args[..], &["main.py"]].concat());
    assert_eq!(sorted(r.findings()), rebased(&restricted(&whole, &["libs/app/ns/app/main.py"]), "libs/app/ns/app"), "{}{}", r.stdout, r.stderr);
    // No positional path: the whole project, wherever it runs from.
    let r = p.check_in("libs/app", &args);
    assert_eq!(sorted(r.findings()), rebased(&whole.findings(), "libs/app"));
    assert!(!r.stdout.contains("Filtered to"), "{}", r.stdout);
    // A path covering the project root filters nothing.
    let r = p.check(&[&args[..], &["."]].concat());
    assert_eq!(r.findings(), whole.findings());
    assert!(!r.stdout.contains("Filtered to"), "{}", r.stdout);
    // Only one cache, in the workspace root.
    assert!(p.path(".pymapper_cache").is_dir());
    assert!(!p.path("libs/app/.pymapper_cache").exists());
    // JSON reports the same restricted set.
    let r = p.check(&["--confidence", "0", "--format", "json", "libs/app"]);
    assert_eq!(r.stdout.matches("\"code\":").count(), restricted(&whole, &["libs/app"]).len(), "{}", r.stdout);
}

/// A `pyproject.toml` shipped inside a package (pandas' wheel) is not a project root.
#[test]
fn nested_pyproject_inside_a_package_is_not_the_root() {
    let p = Project::new("scope-nested");
    p.file("pyproject.toml", "[project]\nname = \"pkg\"\n")
        .file("pkg/__init__.py", "from pkg.core.window.rolling import Rolling\n\n__all__ = [\"Rolling\"]\n")
        .file("pkg/pyproject.toml", "[project]\nname = \"pkg\"\n[build-system]\nbuild-backend = \"mesonpy\"\n")
        .file("pkg/core/__init__.py", "")
        .file("pkg/core/window/__init__.py", "")
        .file("pkg/core/window/rolling.py", "import sys\n\n\nclass Rolling:\n    pass\n")
        .file("pkg/core/window/ewm.py", "def ewm():\n    return 1\n");
    let whole = p.check(&["--confidence", "0"]);
    let all = whole.findings().join("\n");
    assert!(all.contains("pkg/core/window/rolling.py:1:8: PM405"), "{all}");
    for dir in ["pkg/core/window", "pkg", "pkg/core/window/rolling.py"] {
        let r = p.check(&["--confidence", "0", dir]);
        assert_eq!(r.findings(), restricted(&whole, &[dir]), "{dir}\n{}{}", r.stdout, r.stderr);
        assert!(!r.stdout.contains("module core.window") && !r.stdout.contains("`core."), "{dir}: {}", r.stdout);
    }
}

#[test]
fn paths_outside_the_project_and_symlinks() {
    let p = simple_project("scope-outside");
    let other = Project::new("scope-other");
    other.file("pyproject.toml", "[project]\nname = \"other\"\n").file("other/__init__.py", "").file("other/x.py", "import os\n");
    // The first path picks the project: findings are relative to the other project's root.
    let o = other.path("other");
    let r = p.check(&[o.to_str().expect("utf-8")]);
    r.expect_code(1);
    assert!(r.stdout.contains("other/x.py:1:8: PM405"), "{}", r.stdout);
    assert!(!r.stderr.contains("outside the project root"), "{}", r.stderr);
    // A second path outside the first one's project is not checked, and says so.
    let r = p.check(&["app", o.to_str().expect("utf-8")]);
    r.expect_code(0);
    assert!(r.stderr.contains("is outside the project root") && r.stderr.contains("is not checked"), "{}", r.stderr);
    assert!(r.stdout.contains("(3 of 3 first-party files)"), "{}", r.stdout);

    // Symlinks are not followed: neither inside the project nor as a positional path.
    std::os::unix::fs::symlink(other.path("other"), p.path("linked")).expect("symlink");
    let r = p.check(&["--confidence", "0"]);
    assert!(!r.stdout.contains("linked") && !r.stdout.contains("x.py"), "{}", r.stdout);
    let r = p.check(&["linked"]);
    r.expect_code(0);
    assert!(r.stderr.contains("no first-party Python files found under"), "{}", r.stderr);
    assert!(!r.stdout.contains("x.py"), "{}", r.stdout);
}

#[test]
fn malformed_suppressions_are_reported() {
    let p = simple_project("bad-sup");
    p.file(
        "app/util.py",
        "import os  # pymapper: ignore[pm405]\nimport re  # pymapper: ignore[E501]\nimport io  # pymapper: ignore[]\n\
         import sys  # pymapper: ignore[PM999, PM405]\n\n\ndef helper():\n    return 1\n",
    );
    let r = p.check(&["--no-cache"]);
    r.expect_code(1);
    for needle in [
        "app/util.py:1: invalid suppression code `pm405` suppresses nothing; did you mean `PM405`?",
        "app/util.py:2: `E501` is not a pymapper rule code",
        "app/util.py:3: empty `# pymapper: ignore[]` suppresses nothing",
        "app/util.py:4: suppression code `PM999` matches no known rule",
    ] {
        assert!(r.stderr.contains(needle), "{needle}:\n{}", r.stderr);
    }
    // The malformed ones suppress nothing; the valid code next to an unknown one still works.
    assert_eq!(r.codes(), ["PM405", "PM405", "PM405"], "{}", r.stdout);
    assert!(!r.stdout.contains("`sys`"), "{}", r.stdout);
    // Files outside the positional paths are not diagnosed.
    let r = p.check(&["--no-cache", "app/main.py"]);
    assert!(!r.stderr.contains("suppression"), "{}", r.stderr);
}

#[test]
fn entrypoint_patterns_that_match_nothing_warn() {
    let p = monorepo("ep-unmatched");
    let args = ["--no-cache", "--select", "PM401", "--mode", "application"];
    let set = "entrypoints.names = [\"libkg.api\", \"no_such_name\", \"pkg.gone:*\"]";
    let r = p.check(&[&args[..], &["--set", set, "--set", "entrypoints.decorators = [\"never.used\"]"]].concat());
    // A bare module name means `module:*`.
    assert!(!r.stdout.contains("libkg.api:"), "{}", r.stdout);
    for pat in ["no_such_name", "pkg.gone:*", "never.used"] {
        assert!(r.stderr.contains(&format!("entrypoint pattern `{pat}`")), "{pat}:\n{}", r.stderr);
    }
    assert!(!r.stderr.contains("pattern `libkg.api`"), "{}", r.stderr);
    assert_eq!(r.stderr.matches("matches nothing").count(), 3, "{}", r.stderr);
    // No dead-code pass, no matching, no warning.
    let r = p.check(&["--no-cache", "--select", "PM1", "--set", set]);
    assert!(!r.stderr.contains("matches nothing"), "{}", r.stderr);
}

/// Whole-program warnings about units are limited to units overlapping the positional paths.
#[test]
fn unit_warnings_follow_the_positional_paths() {
    let p = monorepo("scope-unit-warn");
    let args = ["--no-cache", "--select", "PM401", "--mode", "application"];
    let r = p.check(&args);
    assert!(r.stderr.contains("no entry points found in") && r.stderr.contains("libs/lib"), "{}", r.stderr);
    let r = p.check(&[&args[..], &["apps/app"]].concat());
    assert!(!r.stderr.contains("no entry points found"), "{}", r.stderr);
    let r = p.check(&[&args[..], &["libs/lib/libkg/api.py"]].concat());
    assert!(r.stderr.contains("no entry points found"), "{}", r.stderr);
}
