//! End-to-end tests of `pymapper deps --check` (PM601-PM604): the real binary on small projects
//! with a venv.

use std::path::PathBuf;
use std::process::Command;
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

const SITE: &str = ".venv/lib/python3.12/site-packages";

impl Project {
    fn new(name: &str) -> Project {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("pymapper-deps-check-{}-{n}-{name}", std::process::id()));
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

    /// A distribution in the venv providing the packages `tops` (each an `__init__.py`), with
    /// `Requires-Dist` headers `requires`.
    fn dist(&self, name: &str, version: &str, tops: &[&str], requires: &[&str]) -> &Project {
        let info = format!("{name}-{version}.dist-info");
        let mut record = String::new();
        for top in tops {
            self.file(&format!("{SITE}/{top}/__init__.py"), "def f():\n    return 1\n");
            record.push_str(&format!("{top}/__init__.py,,\n"));
        }
        record.push_str(&format!("{info}/METADATA,,\n{info}/RECORD,,\n"));
        let mut meta = format!("Metadata-Version: 2.1\nName: {name}\nVersion: {version}\n");
        for r in requires {
            meta.push_str(&format!("Requires-Dist: {r}\n"));
        }
        meta.push_str("\nA long description that mentions Requires-Dist: nothing\n");
        self.file(&format!("{SITE}/{info}/METADATA"), &meta).file(&format!("{SITE}/{info}/RECORD"), &record)
    }

    fn run(&self, args: &[&str]) -> Run {
        let out = Command::new(env!("CARGO_BIN_EXE_pymapper"))
            .args(["deps", "--check", "--no-cache"])
            .args(concise(args))
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

    /// The finding lines (`path:line:col: CODE message`).
    fn findings(&self) -> Vec<&str> {
        self.stdout.lines().filter(|l| l.contains(": PM6")).collect()
    }
}

const PYPROJECT: &str = r#"[project]
name = "app"
version = "0.1"
dependencies = [
    "requests[socks]>=2",
    "PyYAML",
    "idle",
    "toolonly",
    "plugin",  # pymapper: ignore[PM602]
]

[project.optional-dependencies]
fast = ["ujson"]

[dependency-groups]
dev = ["pytest", {include-group = "lint"}]
lint = ["ruff"]
"#;

const APP: &str = "\
import os
import requests
import yaml
import urllib3
from requests._internal import helper
from requests import _private, __version__
import numpy
import ujson
import pytest
from app import util
try:
    import orjson
except ImportError:
    orjson = None
";

/// `app` imports declared (`requests`, `yaml`, `ujson`, `pytest`), transitive (`urllib3`, via
/// requests), undeclared (`numpy`, `orjson` optional), private names of requests; `idle` is
/// declared but unused, `toolonly` installs no importable name, `plugin` is suppressed.
fn project(name: &str) -> Project {
    let p = Project::new(name);
    p.file("pyproject.toml", PYPROJECT)
        .file("app/__init__.py", APP)
        .file("app/util.py", "")
        .file(".venv/pyvenv.cfg", "home = /nonexistent/bin\nversion = 3.12.4\n")
        .dist("requests", "2.0", &["requests"], &["urllib3>=1", "PySocks>=1; extra == \"socks\""])
        .dist("urllib3", "2.0", &["urllib3"], &[])
        .dist("PySocks", "1.7", &["socks"], &[])
        .dist("PyYAML", "6.0", &["yaml"], &[])
        .dist("idle", "1.0", &["idle"], &[])
        .dist("toolonly", "1.0", &[], &[])
        .dist("plugin", "1.0", &["plugin"], &[])
        .dist("numpy", "2.0", &["numpy"], &[])
        .dist("ujson", "5.0", &["ujson"], &[])
        .dist("orjson", "3.0", &["orjson"], &[])
        .dist("pytest", "8.0", &["pytest", "_pytest"], &[]);
    p
}

#[test]
fn reports_undeclared_unused_and_private() {
    let p = project("basic");
    let r = p.run(&[]);
    r.expect_code(1);
    assert_eq!(
        r.findings(),
        [
            "app/__init__.py:4:8: PM601 `urllib3` is provided by `urllib3`, which is not a declared dependency",
            "app/__init__.py:5:6: PM603 `requests._internal` is private to `requests`",
            "app/__init__.py:6:22: PM603 `requests._private` is private to `requests`",
            "app/__init__.py:7:8: PM601 `numpy` is provided by `numpy`, which is not a declared dependency",
            "pyproject.toml:7:5: PM602 dependency `idle` is declared but never imported",
        ],
        "{}",
        r.stdout
    );
    // `toolonly` (no import name) and `orjson` (optional import) are below the gate.
    let all = p.run(&["--min-confidence", "0"]);
    let f = all.findings();
    assert!(f.contains(&"pyproject.toml:8:5: PM602 dependency `toolonly` is declared but never imported (confidence 50)"), "{}", all.stdout);
    assert!(f.iter().any(|l| l.starts_with("app/__init__.py:12:12: PM601 `orjson`")), "{}", all.stdout);
    assert!(!r.stdout.contains("plugin"), "{}", r.stdout);
    // Loading third-party code maps through the loaded groups: the same findings.
    let loaded = p.run(&["--deps"]);
    loaded.expect_code(1);
    assert_eq!(loaded.findings(), r.findings(), "{}", loaded.stderr);
}

#[test]
fn transitive_rule_replaces_undeclared_when_enabled() {
    let p = project("transitive");
    let r = p.run(&["--extend-select", "PM604"]);
    r.expect_code(1);
    let f = r.findings();
    assert!(
        f.contains(&"app/__init__.py:4:8: PM604 `urllib3` is provided by `urllib3`, installed only as a dependency of `requests`"),
        "{}",
        r.stdout
    );
    assert!(!f.iter().any(|l| l.contains("PM601 `urllib3`")), "{}", r.stdout);
    // numpy is required by nobody: still PM601.
    assert!(f.iter().any(|l| l.contains("PM601 `numpy`")), "{}", r.stdout);
}

#[test]
fn extras_gate_requirements() {
    let p = project("extras");
    // PySocks comes with requests[socks] only.
    p.file("app/net.py", "import socks\n");
    let r = p.run(&["--select", "PM604"]);
    r.expect_code(1);
    assert!(r.stdout.contains("app/net.py:1:8: PM604 `socks` is provided by `PySocks`, installed only as a dependency of `requests`"), "{}", r.stdout);
    let text = PYPROJECT.replace("requests[socks]>=2", "requests>=2");
    p.file("pyproject.toml", &text);
    let r = p.run(&["--select", "PM604,PM601"]);
    assert!(r.stdout.contains("app/net.py:1:8: PM601 `socks` is provided by `PySocks`"), "{}", r.stdout);
}

#[test]
fn json_sarif_github_and_exit_codes() {
    let p = project("formats");
    let j = p.run(&["--format", "json", "--select", "PM602"]);
    j.expect_code(1);
    assert!(j.stdout.starts_with("{\"version\":2,"), "{}", j.stdout);
    assert!(j.stdout.contains("\"code\":\"PM602\""), "{}", j.stdout);
    assert!(j.stdout.contains("\"path\":\"pyproject.toml\""), "{}", j.stdout);
    let s = p.run(&["--format", "sarif", "--select", "PM603"]);
    s.expect_code(1);
    assert!(s.stdout.contains("\"ruleId\":\"PM603\""), "{}", s.stdout);
    let g = p.run(&["--format", "github", "--select", "PM601"]);
    g.expect_code(1);
    assert!(g.stdout.contains("::error") && g.stdout.contains("PM601"), "{}", g.stdout);
    // Nothing selected that `--check` runs: a usage error.
    let none = p.run(&["--select", "PM101"]);
    none.expect_code(2);
    assert!(none.stderr.contains("none of the selected rules is a dependency rule"), "{}", none.stderr);
    // `--check` conflicts with the listing flags.
    let bad = p.run(&["--score"]);
    bad.expect_code(2);
}

#[test]
fn suppressions_per_file_ignores_and_baselines() {
    let p = project("filters");
    let r = p.run(&["--set", "rules.per-file-ignores = { \"pyproject.toml\" = [\"PM602\"], \"app/*\" = [\"PM603\"] }"]);
    r.expect_code(1);
    let f = r.findings();
    assert!(!f.iter().any(|l| l.contains("PM602") || l.contains("PM603")), "{}", r.stdout);
    assert!(f.iter().any(|l| l.contains("PM601 `numpy`")), "{}", r.stdout);
    // Inline suppression on the import line.
    let app = APP.replace("import numpy\n", "import numpy  # pymapper: ignore[PM601]\n");
    p.file("app/__init__.py", &app);
    let r = p.run(&[]);
    assert!(!r.stdout.contains("numpy"), "{}", r.stdout);
    // Baselines: save, then nothing new.
    let saved = p.run(&["--save-baseline", "deps-baseline.txt"]);
    saved.expect_code(0);
    assert!(saved.stderr.contains("saved 4 finding(s)"), "{}", saved.stderr);
    p.run(&["--baseline", "deps-baseline.txt"]).expect_code(0);
    p.file("app/extra.py", "import orjson\n");
    let new = p.run(&["--baseline", "deps-baseline.txt"]);
    new.expect_code(1);
    assert_eq!(new.findings(), ["app/extra.py:1:8: PM601 `orjson` is provided by `orjson`, which is not a declared dependency"]);
}

#[test]
fn positional_paths_filter_the_report() {
    let p = project("scope");
    p.file("other/mod.py", "import numpy\n");
    let r = p.run(&["other"]);
    r.expect_code(1);
    assert_eq!(r.findings(), ["other/mod.py:1:8: PM601 `numpy` is provided by `numpy`, which is not a declared dependency"]);
    assert!(r.stdout.contains("Filtered to 1 of 3 first-party files"), "{}", r.stdout);
}

#[test]
fn units_without_declarations_and_dynamic_dependencies() {
    let p = Project::new("nodecl");
    p.file("pyproject.toml", "[tool.ruff]\nline-length = 100\n")
        .file("app/__init__.py", "import numpy\nfrom numpy import _core\n")
        .file(".venv/pyvenv.cfg", "version = 3.12.4\n")
        .dist("numpy", "2.0", &["numpy"], &[]);
    let r = p.run(&[]);
    // PM603 does not need declarations.
    r.expect_code(1);
    assert_eq!(r.findings(), ["app/__init__.py:2:19: PM603 `numpy._core` is private to `numpy`"]);
    assert!(r.stderr.contains("no dependency declarations found"), "{}", r.stderr);
    p.file("pyproject.toml", "[project]\nname = \"app\"\ndynamic = [\"dependencies\"]\n");
    let r = p.run(&["--select", "PM601,PM602"]);
    r.expect_code(0);
    assert!(r.stderr.contains("`dependencies` is dynamic"), "{}", r.stderr);
}

#[test]
fn workspace_members_declare_their_own_dependencies() {
    let p = Project::new("workspace");
    p.file("pyproject.toml", "[project]\nname = \"root\"\ndependencies = [\"lib-a\"]\n[tool.uv.workspace]\nmembers = [\"libs/*\"]\n")
        .file("main.py", "import lib_a\nimport yaml\n")
        .file("libs/lib-a/pyproject.toml", "[project]\nname = \"lib-a\"\ndependencies = [\"PyYAML\"]\n")
        .file("libs/lib-a/src/lib_a/__init__.py", "import yaml\n")
        .file(".venv/pyvenv.cfg", "version = 3.12.4\n")
        .dist("PyYAML", "6.0", &["yaml"], &[]);
    let r = p.run(&[]);
    r.expect_code(1);
    // The root imports yaml without declaring it; lib-a (first-party) is used by the root.
    assert_eq!(r.findings(), ["main.py:2:8: PM601 `yaml` is provided by `PyYAML`, which is not a declared dependency"], "{}", r.stdout);
}

#[test]
fn meta_packages_declare_what_they_require() {
    let p = Project::new("meta");
    // `bundle` installs no module; it pulls in `core-impl` (module `core_impl`) and, with the
    // `extra` extra only, `addon`.
    p.file("pyproject.toml", "[project]\nname = \"app\"\ndependencies = [\"bundle\"]\n")
        .file("app/__init__.py", "import core_impl\n")
        .file(".venv/pyvenv.cfg", "version = 3.12.4\n")
        .dist("bundle", "1.0", &[], &["core-impl==1.0", "addon; extra == \"extra\""])
        .dist("core-impl", "1.0", &["core_impl"], &[])
        .dist("addon", "1.0", &["addon"], &[]);
    let r = p.run(&["--min-confidence", "0"]);
    r.expect_code(0);
    assert!(r.findings().is_empty(), "{}", r.stdout);
    // Not used through what it requires: unused like any tool without modules (confidence 50).
    p.file("app/__init__.py", "import addon\n");
    let r = p.run(&["--min-confidence", "0"]);
    assert_eq!(
        r.findings(),
        [
            "app/__init__.py:1:8: PM601 `addon` is provided by `addon`, which is not a declared dependency",
            "pyproject.toml:3:17: PM602 dependency `bundle` is declared but never imported (confidence 50)",
        ],
        "{}",
        r.stdout
    );
    p.file("pyproject.toml", "[project]\nname = \"app\"\ndependencies = [\"bundle[extra]\"]\n");
    let r = p.run(&["--min-confidence", "0"]);
    r.expect_code(0);
    assert!(r.findings().is_empty(), "{}", r.stdout);
}

#[test]
fn namespace_packages_map_to_the_distribution_listing_the_module() {
    let p = Project::new("namespace");
    p.file("pyproject.toml", "[project]\nname = \"app\"\ndependencies = [\"google-api-core\", \"google-cloud-storage\"]\n")
        .file(
            "app/__init__.py",
            "import google.protobuf\nfrom google.api_core import f\nfrom google.cloud import storage\nfrom google.cloud import bigquery\n",
        )
        .file(".venv/pyvenv.cfg", "version = 3.12.4\n")
        .dist("google-api-core", "2.0", &["google/api_core"], &[])
        .dist("google-cloud-bigquery", "3.0", &["google/cloud/bigquery"], &[])
        .dist("google-cloud-storage", "2.0", &["google/cloud/storage"], &[])
        .dist("protobuf", "5.0", &["google/protobuf"], &[]);
    let want = [
        "app/__init__.py:1:8: PM601 `google.protobuf` is provided by `protobuf`, which is not a declared dependency",
        "app/__init__.py:4:6: PM601 `google.cloud` is provided by `google-cloud-bigquery`, which is not a declared dependency",
    ];
    for args in [&[][..], &["--deps"][..]] {
        let r = p.run(args);
        r.expect_code(1);
        assert_eq!(r.findings(), want, "{args:?}: {}", r.stdout);
    }
}

#[test]
fn no_venv_maps_by_name_below_the_gate() {
    let p = Project::new("novenv");
    p.file("pyproject.toml", "[project]\nname = \"app\"\ndependencies = []\n").file("app/__init__.py", "import yaml\nimport requests\n");
    let r = p.run(&["--min-confidence", "0"]);
    r.expect_code(1);
    assert!(r.stderr.contains("mapped to distributions by name only"), "{}", r.stderr);
    assert_eq!(
        r.findings(),
        [
            "app/__init__.py:1:8: PM601 `yaml` is provided by `PyYAML`, which is not a declared dependency (confidence 90)",
            "app/__init__.py:2:8: PM601 `requests` is provided by `requests`, which is not a declared dependency (confidence 60)",
        ],
        "{}",
        r.stdout
    );
    // The default gate hides the guess only.
    let r = p.run(&[]);
    assert_eq!(r.findings().len(), 1, "{}", r.stdout);
}

#[test]
fn multi_line_from_imports_report_once_per_statement() {
    let p = Project::new("multiline");
    p.file("pyproject.toml", "[project]\nname = \"app\"\ndependencies = [\"requests\"]\n")
        .file(
            "app/__init__.py",
            "import os\nfrom yaml import (\n    safe_load,\n    dump,\n)\nfrom requests._internal import (\n    a,\n    b,\n)\nfrom yaml import load\n",
        )
        .file(".venv/pyvenv.cfg", "version = 3.12.4\n")
        .dist("requests", "2.0", &["requests"], &[])
        .dist("PyYAML", "6.0", &["yaml"], &[]);
    let r = p.run(&[]);
    r.expect_code(1);
    // One finding per statement and distribution, at the module token; the separate
    // `from yaml import load` statement reports on its own.
    assert_eq!(
        r.findings(),
        [
            "app/__init__.py:2:6: PM601 `yaml` is provided by `PyYAML`, which is not a declared dependency",
            "app/__init__.py:6:6: PM603 `requests._internal` is private to `requests`",
            "app/__init__.py:10:6: PM601 `yaml` is provided by `PyYAML`, which is not a declared dependency",
        ],
        "{}",
        r.stdout
    );
    // A suppression on the statement's first line covers every name of it.
    p.file(
        "app/__init__.py",
        "from yaml import (  # pymapper: ignore[PM601]\n    safe_load,\n    dump,\n)\nfrom requests._internal import (  # pymapper: ignore[PM603]\n    a,\n    b,\n)\n",
    );
    let r = p.run(&[]);
    r.expect_code(0);
    assert!(r.findings().is_empty(), "{}", r.stdout);
    // The names of a namespace package may map to several distributions: one finding for
    // each undeclared one.
    p.file("pyproject.toml", "[project]\nname = \"app\"\ndependencies = [\"google-cloud-storage\"]\n")
        .file("app/__init__.py", "from google.cloud import (\n    storage,\n    bigquery,\n    pubsub,\n)\n")
        .dist("google-cloud-bigquery", "3.0", &["google/cloud/bigquery"], &[])
        .dist("google-cloud-pubsub", "2.0", &["google/cloud/pubsub"], &[])
        .dist("google-cloud-storage", "2.0", &["google/cloud/storage"], &[]);
    let r = p.run(&[]);
    r.expect_code(1);
    assert_eq!(
        r.findings(),
        [
            "app/__init__.py:1:6: PM601 `google.cloud` is provided by `google-cloud-bigquery`, which is not a declared dependency",
            "app/__init__.py:1:6: PM601 `google.cloud` is provided by `google-cloud-pubsub`, which is not a declared dependency",
        ],
        "{}",
        r.stdout
    );
}
