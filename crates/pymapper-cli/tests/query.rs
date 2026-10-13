//! End-to-end tests of the query subcommands (`callers`, `callees`, `impact`, `path`,
//! `imports`, `cycles`, `summary`): the real binary on small generated projects.

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
        let dir = std::env::temp_dir().join(format!("pymapper-query-{}-{n}-{name}", std::process::id()));
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

    /// Runs `pymapper <args> --no-cache` in the project directory.
    fn run(&self, args: &[&str]) -> Run {
        self.run_in("", args)
    }

    /// Runs `pymapper <args> --no-cache` in the directory `rel` of the project.
    fn run_in(&self, rel: &str, args: &[&str]) -> Run {
        let out = Command::new(env!("CARGO_BIN_EXE_pymapper"))
            .args(args)
            .arg("--no-cache")
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
    #[track_caller]
    fn expect_code(&self, code: i32) -> &Run {
        assert_eq!(self.code, code, "exit code\nstdout:\n{}\nstderr:\n{}", self.stdout, self.stderr);
        self
    }

    /// Result lines (every stdout line but the last, the summary line).
    fn rows(&self) -> Vec<&str> {
        let lines: Vec<&str> = self.stdout.lines().collect();
        lines[..lines.len().saturating_sub(1)].to_vec()
    }

    /// The summary line (last line of stdout).
    fn summary(&self) -> &str {
        self.stdout.lines().last().unwrap_or("")
    }

    #[track_caller]
    fn has_row(&self, row: &str) -> &Run {
        assert!(self.rows().contains(&row), "missing row {row:?} in\n{}", self.stdout);
        self
    }

    #[track_caller]
    fn lacks(&self, text: &str) -> &Run {
        assert!(!self.stdout.contains(text), "unexpected {text:?} in\n{}", self.stdout);
        self
    }
}

/// A project with two `helper` functions, an override, a lazy and a type-only import, an
/// import cycle in a subpackage, direct recursion and a test calling into the code.
fn sample(name: &str) -> Project {
    let p = Project::new(name);
    p.file("pyproject.toml", "[project]\nname = \"app\"\n")
        .file("app/__init__.py", "")
        .file(
            "app/util.py",
            "def helper():\n    return 1\n\n\nclass Base:\n    def run(self):\n        return helper()\n\n\nclass Child(Base):\n    def run(self):\n        return 2\n",
        )
        .file(
            "app/main.py",
            "from app.util import helper\nfrom app.core.engine import start\n\n\ndef run():\n    x = helper()\n    return helper() + x + start()\n\n\nif __name__ == \"__main__\":\n    run()\n",
        )
        .file(
            "app/cli.py",
            "import os\nfrom typing import TYPE_CHECKING\n\nif TYPE_CHECKING:\n    from app.util import Base\n\n\ndef main():\n    from app.main import run\n    return run()\n\n\ndef helper():\n    return os.getcwd()\n",
        )
        .file("app/core/__init__.py", "")
        .file("app/core/engine.py", "from app.core import db\n\n\ndef start():\n    return db.connect()\n")
        .file(
            "app/core/db.py",
            "from app.core import engine\n\n\ndef connect():\n    return 1\n\n\ndef again():\n    return engine.start()\n\n\ndef walk(n):\n    return walk(n - 1) if n else 0\n",
        )
        .file("tests/test_util.py", "from app.util import helper\n\n\ndef test_helper():\n    assert helper() == 1\n");
    p
}

#[test]
fn callers_one_fact_per_line_at_the_call_site() {
    let p = sample("callers");
    let r = p.run(&["callers", "app.util:helper"]);
    r.expect_code(0)
        .has_row("app/main.py:6:9: app.main:run [certain] +1 site")
        .has_row("app/util.py:7:16: app.util:Base.run [certain]");
    assert!(r.rows().iter().any(|l| l.starts_with("tests/test_util.py:5:12: ") && l.ends_with("test_helper [certain]")), "{}", r.stdout);
    assert_eq!(r.rows().len(), 3, "{}", r.stdout);
    assert_eq!(r.summary(), "3 callers of app.util:helper (function, app/util.py:1)");
    // Dotted names resolve too; depth 2 reaches the module-level caller through `run`.
    let r = p.run(&["callers", "app.util.helper", "--depth", "2"]);
    r.expect_code(0)
        .has_row("app/cli.py:10:12: app.cli:main [certain] depth 2 via app.main:run")
        .has_row("app/main.py:11:5: app.main (module level) [certain] depth 2 via app.main:run");
    assert!(r.summary().ends_with(", up to depth 2"), "{}", r.summary());
    // `--transitive` follows every hop.
    let r = p.run(&["callers", "app.util:helper", "--transitive"]);
    assert_eq!(r.expect_code(0).rows().len(), 5, "{}", r.stdout);
    assert!(r.summary().ends_with(", transitive"), "{}", r.summary());
}

#[test]
fn callees_show_the_definition_and_dispatch() {
    let p = sample("callees");
    p.run(&["callees", "app.main:run"])
        .expect_code(0)
        .has_row("app/main.py:6:9: app.util:helper [certain] +1 site def app/util.py:1:5")
        .has_row("app/main.py:7:27: app.core.engine:start [certain] def app/core/engine.py:4:5");
    // A base method reaches its override through dynamic dispatch only.
    let r = p.run(&["callees", "Base.run"]);
    r.expect_code(0).has_row("app/util.py:7:16: app.util:helper [certain] def app/util.py:1:5");
    assert!(r.rows().iter().any(|l| l.contains("app.util:Child.run [") && l.contains(", dispatch]")), "{}", r.stdout);
    // Without override edges the dispatch row goes.
    p.run(&["callees", "Base.run", "--no-overrides"]).expect_code(0).lacks("Child.run");
    // A module stands for everything in it, edges inside it are never reported.
    let r = p.run(&["callees", "app.core.engine"]);
    r.expect_code(0).has_row("app/core/engine.py:5:12: app.core.db:connect [certain] def app/core/db.py:4:5");
    assert!(r.summary().starts_with("1 callee of app.core.engine (module, app/core/engine.py)"), "{}", r.summary());
}

#[test]
fn ambiguous_and_unknown_symbols_list_candidates_and_exit_2() {
    let p = sample("ambiguous");
    let r = p.run(&["callers", "helper"]);
    r.expect_code(2);
    assert_eq!(r.stdout, "app/cli.py:13:5: app.cli:helper (function)\napp/util.py:1:5: app.util:helper (function)\n");
    assert!(r.stderr.contains("`helper` is ambiguous: 2 matches"), "{}", r.stderr);
    assert!(r.stderr.contains("pass a qualified id such as `app.cli:helper`"), "{}", r.stderr);
    // Qualifying it by one more segment is enough.
    p.run(&["callers", "util.helper"]).expect_code(0);
    // JSON: the candidates as a document on stdout.
    let r = p.run(&["impact", "helper", "--format", "json"]);
    r.expect_code(2);
    assert!(r.stdout.starts_with("{\"version\":2,\"tool\":{\"name\":\"pymapper\",\"version\":"), "{}", r.stdout);
    assert!(r.stdout.contains("\"query\":\"impact\",\"error\":\"ambiguous\",\"input\":\"helper\",\"candidates\":[\n"), "{}", r.stdout);
    assert!(r.stdout.contains("{\"symbol\":\"app.util:helper\",\"kind\":\"function\",\"path\":\"app/util.py\",\"line\":1,\"col\":5,\"origin\":\"first-party\",\"match\":\"suffix\"}"), "{}", r.stdout);
    // Unknown: similar names, still exit 2.
    let r = p.run(&["callees", "hlper"]);
    r.expect_code(2).has_row("app/cli.py:13:5: app.cli:helper (function)");
    assert!(r.stderr.contains("no symbol matches `hlper`; 2 similar names listed on stdout"), "{}", r.stderr);
    let r = p.run(&["callees", "zzzzzzzz"]);
    r.expect_code(2);
    assert!(r.stdout.is_empty(), "{}", r.stdout);
    assert!(r.stderr.contains("no symbol matches `zzzzzzzz`"), "{}", r.stderr);
}

#[test]
fn json_follows_the_check_conventions() {
    let p = sample("json");
    let r = p.run(&["callers", "app.util:helper", "--format", "json"]);
    r.expect_code(0);
    let lines: Vec<&str> = r.stdout.lines().collect();
    assert_eq!(
        lines[0],
        format!(
            "{{\"version\":2,\"tool\":{{\"name\":\"pymapper\",\"version\":\"{}\"}},\"query\":\"callers\",\"target\":{{\"symbol\":\"app.util:helper\",\"kind\":\"function\",\"path\":\"app/util.py\",\"line\":1,\"col\":5}},\"depth\":1,\"results\":[",
            env!("CARGO_PKG_VERSION")
        )
    );
    assert!(lines.contains(&"  {\"symbol\":\"app.main:run\",\"kind\":\"function\",\"path\":\"app/main.py\",\"line\":5,\"col\":5,\"confidence\":\"certain\",\"path_confidence\":\"certain\",\"dispatch\":false,\"depth\":1,\"via\":\"app.util:helper\",\"site\":{\"path\":\"app/main.py\",\"line\":6,\"col\":9},\"sites\":2},"), "{}", r.stdout);
    assert_eq!(lines.last(), Some(&"],\"summary\":{\"results\":3,\"shown\":3,\"outside_paths\":0,\"truncated\":false}}"));
    // Every subcommand writes one document with the same header.
    for args in [
        &["callees", "app.main:run"][..],
        &["impact", "app.util:helper"],
        &["path", "app.cli:main", "app.util:helper"],
        &["imports", "app.cli"],
        &["cycles"],
        &["summary"],
    ] {
        let mut a = args.to_vec();
        a.extend(["--format", "json"]);
        let r = p.run(&a);
        r.expect_code(0);
        let head = format!("{{\"version\":2,\"tool\":{{\"name\":\"pymapper\",\"version\":\"{}\"}},\"query\":\"{}\"", env!("CARGO_PKG_VERSION"), args[0]);
        assert!(r.stdout.starts_with(&head), "{args:?}: {}", r.stdout);
        assert!(r.stdout.ends_with("}\n"), "{args:?}: {}", r.stdout);
        assert_eq!(r.stdout.matches('{').count(), r.stdout.matches('}').count(), "{args:?}: {}", r.stdout);
    }
    // Formats other than concise and json are usage errors.
    let r = p.run(&["callers", "app.util:helper", "--format", "sarif"]);
    r.expect_code(2);
    assert!(r.stderr.contains("writes the human, llm, concise and json formats, not `sarif`"), "{}", r.stderr);
}

#[test]
fn positional_paths_filter_rows_and_never_reroot() {
    let p = sample("paths");
    let whole = p.run(&["callers", "app.util:helper", "--transitive"]);
    whole.expect_code(0);
    // Only rows under the paths; the summary says how many were hidden.
    let r = p.run(&["callers", "app.util:helper", "--transitive", "app/cli.py", "tests"]);
    r.expect_code(0);
    let want: Vec<&str> = whole.rows().into_iter().filter(|l| l.starts_with("app/cli.py:") || l.starts_with("tests/")).collect();
    assert_eq!(r.rows(), want);
    assert!(r.summary().ends_with("; 3 outside PATHS not shown"), "{}", r.summary());
    // From a subdirectory: the same root, module names and rows, with paths relative to the
    // working directory (ISSUES O6; 0.3.0 showed them relative to the project root).
    let sub = p.run_in("app/core", &["callers", "app.util:helper", "--transitive", "../cli.py", "../../tests"]);
    let want = r.stdout.replace("app/cli.py", "../cli.py").replace("tests/", "../../tests/").replace("app/util.py", "../util.py");
    assert_eq!(sub.expect_code(0).stdout, want);
    // JSON keeps paths relative to the project root.
    let json = |dir: &str| p.run_in(dir, &["callers", "app.util:helper", "--format", "json"]).stdout;
    assert_eq!(json("app/core"), json(""));
    // A path covering the project root filters nothing.
    assert_eq!(p.run(&["callers", "app.util:helper", "--transitive", "."]).stdout, whole.stdout);
    // A path that does not exist is a usage error; one without Python files warns.
    let r = p.run(&["callers", "app.util:helper", "nope"]);
    r.expect_code(2);
    assert!(r.stderr.contains("path `") && r.stderr.contains("does not exist"), "{}", r.stderr);
    p.file("docs/readme.txt", "hi\n");
    let r = p.run(&["callers", "app.util:helper", "docs"]);
    r.expect_code(0);
    assert!(r.stderr.contains("no loaded Python files under"), "{}", r.stderr);
    assert!(r.rows().is_empty(), "{}", r.stdout);
}

#[test]
fn impact_counts_affected_code_tests_and_importers() {
    let p = sample("impact");
    let r = p.run(&["impact", "app.util:helper"]);
    r.expect_code(0)
        .has_row("app/main.py:6:9: app.main:run [certain] +1 site")
        .has_row("app/cli.py:10:12: app.cli:main [certain] depth 2 via app.main:run")
        // Importers at the import statement of the module that brought them in.
        .has_row("app/main.py:1:22: app.main [importer] imports app.util")
        .has_row("app/cli.py:5:26: app.cli [importer] imports app.util");
    let s = r.summary();
    assert!(s.starts_with("impact of app.util:helper (function, app/util.py:1): 5 affected (4 symbols, 1 module level), 1 in tests, 4 files;"), "{s}");
    assert!(s.contains("importing modules; most affected: "), "{s}");
    // --depth bounds the walk.
    let r = p.run(&["impact", "app.util:helper", "--depth", "1"]);
    assert!(r.expect_code(0).summary().contains(": 3 affected (3 symbols, 0 module level)"), "{}", r.summary());
    // JSON: rows, importers, per-module counts and the summary.
    let r = p.run(&["impact", "app.util:helper", "--format", "json"]);
    r.expect_code(0);
    assert!(r.stdout.contains("],\"importers\":[\n"), "{}", r.stdout);
    assert!(r.stdout.contains("],\"modules\":[\n"), "{}", r.stdout);
    assert!(r.stdout.contains("\"summary\":{\"results\":5,\"symbols\":4,\"module_scopes\":1,\"tests\":1,\"files\":4,"), "{}", r.stdout);
    assert!(
        r.stdout.contains("\"symbol\":\"app.main\",\"kind\":\"module\",\"path\":\"app/main.py\",\"line\":null,\"col\":null,\"imports\":\"app.util\",\"direct\":true,\"site\":{\"path\":\"app/main.py\",\"line\":1,\"col\":22},\"rows\":1}"),
        "{}",
        r.stdout
    );
    // Transitive importers name the module that brought them in.
    let r = p.run(&["impact", "app.core.db"]);
    r.expect_code(0)
        .has_row("app/core/engine.py:1:22: app.core.engine [importer] imports app.core.db")
        .has_row("app/main.py:2:29: app.main [importer] imports app.core.engine")
        .has_row("app/cli.py:9:26: app.cli [importer] imports app.main");
    // --limit caps call rows and importer rows together.
    let r = p.run(&["impact", "app.util:helper", "--limit", "1"]);
    assert_eq!(r.expect_code(0).rows().len(), 1, "{}", r.stdout);
    assert!(r.summary().ends_with("; first 1 shown (--limit 0 for all)"), "{}", r.summary());
    let r = p.run(&["impact", "app.util:helper", "--limit", "6"]);
    assert_eq!(r.expect_code(0).rows().len(), 6, "{}", r.stdout);
    assert!(r.rows()[5].ends_with(" [importer] imports app.util"), "{}", r.stdout);
}

#[test]
fn path_finds_a_shortest_chain_or_exits_1() {
    let p = sample("path");
    let r = p.run(&["path", "app.cli:main", "app.util:helper"]);
    r.expect_code(0);
    assert_eq!(r.rows(), ["app/cli.py:10:12: app.cli:main -> app.main:run [certain]", "app/main.py:6:9: app.main:run -> app.util:helper [certain]"]);
    assert_eq!(
        r.summary(),
        "call path from app.cli:main (function, app/cli.py:8) to app.util:helper (function, app/util.py:1): 2 edges, least sure certain"
    );
    let r = p.run(&["path", "app.util:helper", "app.cli:main"]);
    r.expect_code(1);
    assert_eq!(r.stdout, "no call path from app.util:helper (function, app/util.py:1) to app.cli:main (function, app/cli.py:8)\n");
    let r = p.run(&["path", "app.cli:main", "app.util:helper", "--max-len", "1"]);
    assert!(r.expect_code(1).stdout.ends_with(" within 1 edge\n"), "{}", r.stdout);
    // Imports: one line per import statement; a def stands for its module.
    let r = p.run(&["path", "app.cli", "app.core.db", "--imports"]);
    r.expect_code(0);
    assert_eq!(r.rows().len(), 3, "{}", r.stdout);
    assert!(r.rows()[0].starts_with("app/cli.py:9:"), "{}", r.stdout);
    assert!(r.rows()[0].contains("app.cli -> app.main [import"), "{}", r.stdout);
    let r = p.run(&["path", "app.cli:main", "app.core.db", "--imports", "--format", "json"]);
    r.expect_code(0);
    assert!(r.stdout.contains("\"graph\":\"imports\""), "{}", r.stdout);
    assert!(r.stdout.contains("\"found\":true"), "{}", r.stdout);
    assert!(r.stdout.contains("\"summary\":{\"length\":3,\"worst\":null}"), "{}", r.stdout);
    // --root points discovery at a project outside the working directory.
    let q = Project::new("path-root");
    q.file("proj/pyproject.toml", "[project]\nname = \"m\"\n")
        .file("proj/m.py", "def g():\n    return 1\n\n\ndef f():\n    return g()\n")
        .file("other/notes.txt", "");
    let r = q.run_in("other", &["path", "m:f", "m:g", "--root", "../proj"]);
    // Paths are relative to the working directory (ISSUES O6).
    assert_eq!(r.expect_code(0).rows(), ["../proj/m.py:6:12: m:f -> m:g [certain]"]);
    q.run_in("other", &["path", "m:f", "m:g"]).expect_code(2);
}

#[test]
fn imports_in_and_out_with_rollup() {
    let p = sample("imports");
    let r = p.run(&["imports", "app.cli"]);
    // Every row at its import statement; out rows also name where the module is.
    r.expect_code(0)
        .has_row("app/cli.py:9:26: imports app.main (1 row, lazy) def app/main.py")
        .has_row("app/cli.py:5:26: imports app.util (1 row, type-only, conditional) def app/util.py")
        .has_row("app/cli.py:1:8: imports os (stdlib, not loaded, 1 row)")
        .has_row("app/cli.py:2:20: imports typing (stdlib, not loaded, 1 row)");
    assert_eq!(r.summary(), "imports of app.cli (module, app/cli.py): 4 out (2 not loaded), 0 in");
    // --runtime drops imports only type checkers see.
    p.run(&["imports", "app.cli", "--runtime"]).expect_code(0).lacks("app.util");
    // A package covers its modules: imports between them are inside.
    let r = p.run(&["imports", "app.core"]);
    r.expect_code(0);
    assert_eq!(r.rows(), ["app/main.py:2:29: imported by app.main (1 row)"]);
    // --depth rolls the other side up, never to an ancestor of the target.
    let r = p.run(&["imports", "app.main", "--depth", "1"]);
    r.expect_code(0)
        .has_row("app/main.py:1:22: imports app.util (1 row) def app/util.py")
        .has_row("app/main.py:2:29: imports app.core (1 row) def app/core/__init__.py")
        .has_row("app/cli.py:9:26: imported by app.cli (1 row, lazy)");
    assert!(r.summary().contains(" at depth 1: 2 out, 1 in"), "{}", r.summary());
    let r = p.run(&["imports", "test_util", "--depth", "1", "--direction", "out"]);
    r.expect_code(0);
    assert_eq!(r.rows(), ["tests/test_util.py:1:22: imports app (1 row) def app/__init__.py"]);
    assert!(r.summary().ends_with(": 1 out"), "{}", r.summary());
    // A function: the imports inside it, no incoming side.
    let r = p.run(&["imports", "app.cli:main"]);
    r.expect_code(0);
    assert_eq!(r.stdout, "app/cli.py:9:26: imports app.main (1 row, lazy) def app/main.py\nimports of app.cli:main (function, app/cli.py:8): 1 out\n");
    // Nothing imports a def: asking for its importers is a usage error.
    let r = p.run(&["imports", "app.cli:main", "--direction", "in"]);
    r.expect_code(2);
    assert!(r.stderr.contains("nothing imports a function"), "{}", r.stderr);
    // A runtime import is preferred over a type-only one of the same edge under --runtime.
    let q = Project::new("imports-runtime");
    q.file("pyproject.toml", "[project]\nname = \"rt\"\n")
        .file("rt/__init__.py", "")
        .file("rt/b.py", "X = 1\n")
        .file("rt/a.py", "from typing import TYPE_CHECKING\n\nif TYPE_CHECKING:\n    from rt import b\n\nimport rt.b\n");
    q.run(&["imports", "rt.a", "--direction", "out"]).expect_code(0).has_row("rt/a.py:4:20: imports rt.b (2 rows) def rt/b.py");
    q.run(&["imports", "rt.a", "--direction", "out", "--runtime"]).expect_code(0).has_row("rt/a.py:6:8: imports rt.b (2 rows) def rt/b.py");
    // JSON.
    let r = p.run(&["imports", "app.core.db", "--format", "json", "--direction", "in"]);
    r.expect_code(0);
    assert!(r.stdout.contains("\n  {\"direction\":\"in\",\"symbol\":\"app.core.engine\",\"kind\":\"module\",\"path\":\"app/core/engine.py\",\"line\":null,\"col\":null,\"origin\":\"first-party\",\"rows\":1,\"flags\":[],\"site\":{\"path\":\"app/core/engine.py\",\"line\":1,\"col\":22}}\n"), "{}", r.stdout);
    assert!(r.stdout.contains("\"summary\":{\"out\":null,\"external\":null,\"in\":1,"), "{}", r.stdout);
    // Paths filter the modules listed; imports of code that is not loaded have no path.
    let r = p.run(&["imports", "app.cli", "app/main.py"]);
    r.expect_code(0);
    assert_eq!(r.rows(), ["app/cli.py:9:26: imports app.main (1 row, lazy) def app/main.py"]);
    assert!(r.summary().ends_with("; 3 outside PATHS not shown"), "{}", r.summary());
}

#[test]
fn cycles_of_imports_and_calls() {
    let p = sample("cycles");
    let r = p.run(&["cycles"]);
    r.expect_code(0);
    assert_eq!(r.rows().len(), 1, "{}", r.stdout);
    let row = r.rows()[0];
    assert!(row.starts_with("app/core/"), "{row}");
    assert!(row.contains(": import cycle (2 modules): app.core."), "{row}");
    assert!(row.contains("app.core.db") && row.contains("app.core.engine"), "{row}");
    assert_eq!(r.summary(), "1 import cycle");
    // Through a symbol: the same component (its witness may start elsewhere).
    let r2 = p.run(&["cycles", "--symbol", "app.core.engine:start"]);
    assert_eq!(r2.expect_code(0).rows().len(), 1, "{}", r2.stdout);
    assert!(r2.rows()[0].contains(": import cycle (2 modules): app.core."), "{}", r2.stdout);
    assert!(r2.summary().starts_with("1 import cycle through app.core.engine:start"), "{}", r2.stdout);
    let r = p.run(&["cycles", "--symbol", "app.main"]);
    assert_eq!(r.expect_code(0).stdout, "no import cycles through app.main (module, app/main.py)\n");
    let r = p.run(&["cycles", "app/cli.py"]);
    assert_eq!(r.expect_code(0).stdout, "no import cycles; 1 outside PATHS not shown\n");
    // Recursion through a function.
    let r = p.run(&["cycles", "--symbol", "walk", "--calls"]);
    r.expect_code(0);
    assert_eq!(
        r.stdout,
        "app/core/db.py:13:12: call cycle (1 function): app.core.db:walk -> app.core.db:walk [certain]\n1 call cycle through app.core.db:walk (function, app/core/db.py:12)\n"
    );
    let r = p.run(&["cycles", "--symbol", "connect", "--calls"]);
    assert!(r.expect_code(0).stdout.starts_with("no call cycles through app.core.db:connect"), "{}", r.stdout);
    let r = p.run(&["cycles", "--format", "json"]);
    r.expect_code(0);
    assert!(r.stdout.contains("\"graph\":\"imports\",\"target\":null,\"results\":[\n  {\"size\":2,\"members\":[\"app.core."), "{}", r.stdout);
    assert!(r.stdout.contains("\"summary\":{\"cycles\":1,\"largest\":2,"), "{}", r.stdout);
}

#[test]
fn summary_counts_and_rankings() {
    let p = sample("summary");
    let r = p.run(&["summary"]);
    r.expect_code(0);
    let lines: Vec<&str> = r.stdout.lines().collect();
    assert!(lines[0].starts_with("first-party: 8 files, "), "{}", r.stdout);
    // `tests/` is a namespace package.
    assert!(lines[0].ends_with(", 3 packages, 6 modules, 2 classes, 11 functions"), "{}", r.stdout);
    assert!(lines.iter().any(|l| l.starts_with("imports: ") && l.contains(" 0 unresolved (0.0%)")), "{}", r.stdout);
    assert!(lines.iter().any(|l| l.starts_with("calls: ") && l.contains("caller-callee pairs")), "{}", r.stdout);
    assert!(lines.contains(&"import cycles: 1 (largest: 2 modules)"), "{}", r.stdout);
    assert!(lines.contains(&"app/util.py:1:5: most called app.util:helper (3 callers)"), "{}", r.stdout);
    // Tests are not ranked by default.
    assert!(!r.stdout.contains("test_helper"), "{}", r.stdout);
    // Paths restrict the rankings, not the counts.
    let r = p.run(&["summary", "app/core"]);
    r.expect_code(0);
    assert!(r.stdout.starts_with("first-party: 8 files, "), "{}", r.stdout);
    for l in r.stdout.lines().filter(|l| l.contains(" most ") || l.contains(" cognitive ") || l.contains(" cyclomatic ")) {
        assert!(l.starts_with("app/core/"), "{l}");
    }
    let r = p.run(&["summary", "--top", "1", "--format", "json"]);
    r.expect_code(0);
    assert!(r.stdout.contains("\"origins\":{\"first-party\":{\"files\":8,"), "{}", r.stdout);
    assert!(r.stdout.contains("\"import_cycles\":{\"count\":1,\"largest\":2}"), "{}", r.stdout);
    assert!(r.stdout.contains("\"most_called\":[\n  {\"symbol\":\"app.util:helper\","), "{}", r.stdout);
}

#[test]
fn config_flags_limit_stats_and_closed_stdout() {
    let p = sample("flags");
    // Shared configuration flags change what is loaded.
    p.run(&["callers", "app.main:run", "--exclude", "app/cli.py"]).expect_code(0).lacks("app.cli");
    let r = p.run(&["callers", "app.util:helper", "--set", "extend-exclude = [\"tests\"]"]);
    assert_eq!(r.expect_code(0).rows().len(), 2, "{}", r.stdout);
    p.file("alt.toml", "output-format = \"json\"\n");
    let r = p.run(&["callers", "app.util:helper", "--config", "alt.toml"]);
    assert!(r.expect_code(0).stdout.starts_with("{\"version\":2,"), "{}", r.stdout);
    p.run(&["callers", "app.util:helper", "--isolated", "-j", "1"]).expect_code(0);
    let r = p.run(&["callers", "app.util:helper", "--set", "nonsense = 1"]);
    r.expect_code(2);
    // --limit caps the rows, closest first.
    let r = p.run(&["callers", "app.util:helper", "--transitive", "--limit", "2"]);
    r.expect_code(0);
    assert_eq!(r.rows().len(), 2, "{}", r.stdout);
    assert!(r.summary().starts_with("5 callers of "), "{}", r.summary());
    assert!(r.summary().ends_with("; first 2 shown (--limit 0 for all)"), "{}", r.summary());
    // --stats goes to stderr and leaves stdout alone.
    let plain = p.run(&["callees", "app.main:run"]);
    let r = p.run(&["callees", "app.main:run", "--stats"]);
    assert_eq!(r.expect_code(0).stdout, plain.stdout);
    assert!(r.stderr.contains("pymapper: stats") && r.stderr.contains("query.build") && r.stderr.contains("peak_rss"), "{}", r.stderr);
    // A reader that goes away early is not an error.
    let many: String = (0..3000).map(|i| format!("def caller_{i}():\n    return helper()\n\n\n")).collect();
    p.file("app/many.py", &format!("from app.util import helper\n\n\n{many}"));
    let mut child = Command::new(env!("CARGO_BIN_EXE_pymapper"))
        .args(["callers", "app.util:helper", "--limit", "0", "--no-cache"])
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
    assert_eq!(o.status.code(), Some(0), "{stderr}");
    assert!(!stderr.contains("error") && !stderr.contains("panicked"), "{stderr}");
}
