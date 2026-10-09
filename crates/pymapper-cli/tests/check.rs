//! End-to-end tests of `pymapper check`: the real binary on small generated projects.

use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU32, Ordering};

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
        let out = Command::new(env!("CARGO_BIN_EXE_pymapper"))
            .arg("check")
            .args(args)
            .current_dir(&self.dir)
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
    let r = p.check(&["--select", "PM101"]);
    assert_eq!(r.expect_code(1).codes(), ["PM101", "PM101"]);
    p.file("pymapper.toml", "[rules.per-file-ignores]\n\"tests/\" = [\"PM1\"]\n");
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
    assert!(first.contains("import cycle: "), "{}", r.stdout);

    let r = p.check(&["--select", "PM301", "--format", "json"]);
    r.expect_code(1);
    assert!(r.stdout.starts_with("{\"version\":1,"), "{}", r.stdout);
    assert!(r.stdout.contains("\"code\":\"PM301\""), "{}", r.stdout);
    assert!(r.stdout.contains("\"path\":\"app/"), "{}", r.stdout);

    let r = p.check(&["--select", "PM301", "--output-format", "sarif"]);
    r.expect_code(1);
    assert!(r.stdout.contains("\"version\":\"2.1.0\"") || r.stdout.contains("\"version\": \"2.1.0\""), "{}", r.stdout);
    assert!(r.stdout.contains("PM301"), "{}", r.stdout);

    let r = p.check(&["--select", "PM301", "--format", "github"]);
    r.expect_code(1);
    assert!(r.stdout.lines().any(|l| l.starts_with("::") && l.contains("file=app/")), "{}", r.stdout);

    // The format can come from the config file too.
    p.file("pymapper.toml", "output-format = \"json\"\n");
    let r = p.check(&["--select", "PM301"]);
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
    assert!(!p.path("baseline.json.tmp").exists());

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
        .args(["check", "--no-cache"])
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
    r.expect_code(0);
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
