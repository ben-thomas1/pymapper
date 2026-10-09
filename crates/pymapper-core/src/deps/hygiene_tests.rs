//! Tests of dependency hygiene: `pyproject.toml` declarations, requirement names, TOML comment
//! suppressions, private names, and [`analyse`] on small projects with a venv (provider
//! mapping, confidences). The command (output formats, suppressions, baselines, exit codes) is
//! covered by `crates/pymapper-cli/tests/deps_check.rs`.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use super::declared::{DeclStatus, Declarations, DepKind, parse_requirement, toml_suppressions};
use super::hygiene::{HygieneContext, HygieneCtx, HygieneRules, analyse, is_private};
use crate::index::{Inputs, NONE, load};
use crate::intern::Interner;
use crate::output::{Context, IndexContext};
use crate::resolve::{Attribution, resolve_imports};
use crate::roots::{self, ProjectUnits, RootOptions};
use crate::rules::{BadCodeKind, FileId, Findings, Rule};
use crate::walk::PathList;

/// Declarations of units given as `(text, parent)`; `None` text: no `pyproject.toml`.
fn decls(units: &[(Option<&str>, u32)]) -> (Declarations, Interner) {
    let mut names = Interner::new();
    let texts = units
        .iter()
        .enumerate()
        .map(|(i, (t, _))| (PathBuf::from(format!("/p/u{i}/pyproject.toml")), Ok(t.map(str::to_owned))))
        .collect();
    let parents: Vec<u32> = units.iter().map(|&(_, p)| p).collect();
    let d = Declarations::from_texts(texts, &parents, 100, &mut names);
    (d, names)
}

fn s(names: &Interner, sym: crate::intern::Sym) -> String {
    String::from_utf8_lossy(names.resolve(sym)).into_owned()
}

const FULL: &str = r#"[project]
name = "My_App"
dependencies = [
    "requests[socks, Use_Chardet]>=2",
    'PyYAML ; python_version >= "3.9"',
    "zope.interface @ https://example.com/z.whl",
    "  # not a requirement",
]
[project.optional-dependencies]
fast = ["ujson>=5"]
[dependency-groups]
dev = ["pytest>=8", { include-group = "lint" }]
lint = ["ruff"]
[tool.uv]
dev-dependencies = ["mypy"]
[tool.pdm.dev-dependencies]
docs = ["sphinx"]
"#;

#[test]
fn pep621_and_pep735_declarations() {
    let (d, names) = decls(&[(Some(FULL), NONE)]);
    let u = d.unit(0);
    assert_eq!(u.status, DeclStatus::Declared);
    assert!(!u.dynamic_optional && u.error.is_none());
    assert_eq!(u.project.map(|p| s(&names, p)).as_deref(), Some("my-app"));
    let got: Vec<(String, String, DepKind, u32, u32)> =
        d.deps(0).iter().map(|x| (s(&names, x.name), s(&names, x.norm), x.kind, x.line, x.col)).collect();
    let want = [
        ("requests", "requests", DepKind::Main, 4, 5),
        ("PyYAML", "pyyaml", DepKind::Main, 5, 5),
        ("zope.interface", "zope-interface", DepKind::Main, 6, 5),
        ("ujson", "ujson", DepKind::Optional, 10, 9),
        ("pytest", "pytest", DepKind::Group, 12, 8),
        ("ruff", "ruff", DepKind::Group, 13, 9),
        ("mypy", "mypy", DepKind::Group, 15, 21),
        ("sphinx", "sphinx", DepKind::Group, 17, 9),
    ];
    let got: Vec<(&str, &str, DepKind, u32, u32)> = got.iter().map(|(a, b, k, l, c)| (a.as_str(), b.as_str(), *k, *l, *c)).collect();
    assert_eq!(got, want);
    let extras: Vec<String> = d.extras(&d.deps(0)[0]).iter().map(|&e| s(&names, e)).collect();
    assert_eq!(extras, ["socks", "use-chardet"]);
    let pyyaml = names.get(b"pyyaml").expect("interned");
    assert_eq!(d.find(0, pyyaml).map(|x| x.line), Some(5));
    assert!(d.find(1, pyyaml).is_none());
    assert_eq!(d.file_id(0), FileId(100));
    assert_eq!(d.unit_of_file(FileId(100)), Some(0));
    assert_eq!(d.unit_of_file(FileId(99)), None);
    assert_eq!(d.unit_of_file(FileId(101)), None);
}

#[test]
fn poetry_declarations() {
    let text = r#"[tool.poetry]
name = "legacy"
[tool.poetry.dependencies]
python = "^3.10"
Django = "^5"
celery = { version = "^5", extras = ["redis"], optional = true }
[tool.poetry.dev-dependencies]
black = "*"
[tool.poetry.group.test.dependencies]
pytest = "*"
"#;
    let (d, names) = decls(&[(Some(text), NONE)]);
    assert_eq!(d.unit(0).status, DeclStatus::Declared);
    assert_eq!(d.unit(0).project.map(|p| s(&names, p)).as_deref(), Some("legacy"));
    let got: Vec<(String, DepKind, u32)> = d.deps(0).iter().map(|x| (s(&names, x.norm), x.kind, x.line)).collect();
    assert_eq!(
        got,
        [
            ("django".to_owned(), DepKind::Main, 5),
            ("celery".to_owned(), DepKind::Optional, 6),
            ("black".to_owned(), DepKind::Group, 8),
            ("pytest".to_owned(), DepKind::Group, 10),
        ]
    );
    let extras: Vec<String> = d.extras(&d.deps(0)[1]).iter().map(|&e| s(&names, e)).collect();
    assert_eq!(extras, ["redis"]);
}

#[test]
fn poetry2_dynamic_dependencies_are_read_from_the_poetry_table() {
    let text = "[project]\nname = \"p2\"\ndynamic = [\"dependencies\"]\n[tool.poetry.dependencies]\nrequests = \"^2\"\n";
    let (d, names) = decls(&[(Some(text), NONE)]);
    assert_eq!(d.unit(0).status, DeclStatus::Declared);
    let got: Vec<(String, DepKind)> = d.deps(0).iter().map(|x| (s(&names, x.norm), x.kind)).collect();
    assert_eq!(got, [("requests".to_owned(), DepKind::Main)]);
    // Without a poetry list the dependencies stay unknown.
    let (d, _) = decls(&[(Some("[project]\nname = \"p\"\ndynamic = [\"dependencies\"]\n[tool.poetry]\nname = \"p\"\n"), NONE)]);
    assert_eq!(d.unit(0).status, DeclStatus::Dynamic);
}

#[test]
fn uv_workspace_root_groups() {
    let root = "[project]\nname = \"root\"\ndependencies = [\"lib-a\"]\n[dependency-groups]\ndev = [\"pytest\"]\n\
                [tool.uv]\ndev-dependencies = [\"mypy\"]\n[tool.uv.workspace]\nmembers = [\"libs/*\"]\n";
    let (d, names) = decls(&[
        (Some(root), NONE),
        (Some("[project]\nname = \"lib-a\"\ndependencies = []\n"), 0),
        // A nested unit below a member still sees the workspace root.
        (Some("[project]\nname = \"inner\"\ndependencies = []\n"), 1),
    ]);
    assert!(d.unit(0).workspace_root && !d.unit(1).workspace_root);
    let ws: Vec<u32> = (0..3).map(|u| d.workspace(u)).collect();
    assert_eq!(ws, [NONE, 0, 0]);
    let sym = |n: &str| names.get(n.as_bytes()).expect("interned");
    assert!(d.declares_group(0, sym("pytest")) && d.declares_group(0, sym("mypy")));
    // Main dependencies of the root are not development groups.
    assert!(!d.declares_group(0, sym("lib-a")));
    assert!(!d.declares_group(1, sym("pytest")));
    // No workspace table: no workspace root.
    let (d, _) = decls(&[(Some("[project]\nname = \"r\"\n"), NONE), (Some("[project]\nname = \"m\"\n"), 0)]);
    assert_eq!(d.workspace(1), NONE);
}

#[test]
fn status_and_inheritance() {
    let (d, _) = decls(&[
        // 0: the root declares.
        (Some("[project]\nname = \"root\"\ndependencies = []\n"), NONE),
        // 1: tool config only: inherits the root.
        (Some("[tool.ruff]\nline-length = 100\n"), 0),
        // 2: dynamic dependencies.
        (Some("[project]\nname = \"dyn\"\ndynamic = [\"dependencies\", \"optional-dependencies\"]\n"), 0),
        // 3: below the dynamic unit, no file: inherits the dynamic one.
        (None, 2),
        // 4: invalid TOML: inherits the root.
        (Some("[project\n"), 0),
    ]);
    assert_eq!(d.unit(0).status, DeclStatus::Declared);
    assert_eq!(d.unit(1).status, DeclStatus::None);
    assert_eq!(d.unit(2).status, DeclStatus::Dynamic);
    assert!(d.unit(2).dynamic_optional);
    assert!(d.unit(4).error.as_deref().is_some_and(|e| e.starts_with("invalid TOML")));
    let eff: Vec<u32> = (0..5).map(|u| d.effective(u)).collect();
    assert_eq!(eff, [0, 0, 2, 2, 0]);
    let (none, _) = decls(&[(None, NONE), (Some("[tool.black]\n"), 0)]);
    assert_eq!((none.effective(0), none.effective(1)), (NONE, NONE));
    assert_eq!(none.effective(7), NONE);
}

#[test]
fn requirement_names_and_extras() {
    let cases: &[(&str, Option<(&str, &str)>)] = &[
        ("requests", Some(("requests", ""))),
        ("requests>=2,<3", Some(("requests", ""))),
        ("requests [socks,security] >=2", Some(("requests", "socks,security"))),
        ("zope.interface@https://x/y.whl", Some(("zope.interface", ""))),
        ("Foo_Bar-baz.qux; os_name == 'nt'", Some(("Foo_Bar-baz.qux", ""))),
        ("  spaced", Some(("spaced", ""))),
        ("-leading", None),
        ("trailing-", None),
        ("", None),
        ("pkg[unclosed", None),
    ];
    for &(req, want) in cases {
        assert_eq!(parse_requirement(req), want, "{req:?}");
    }
}

#[test]
fn toml_comment_suppressions() {
    let text = br#"[project]
dependencies = [
    "idle",  # pymapper: ignore[PM602]
    "url#fragment",
    "plain",  # pymapper: ignore
    "multi",  # pymapper: ignore[PM601, XYZ, PM999]
]
description = """
# pymapper: ignore[PM602] inside a string
"""
other = 'lit # pymapper: ignore[PM602]'
escaped = "quote \" # pymapper: ignore[PM602]"
after = "x"  # noqa: F401
"#;
    let mut out = Vec::new();
    let mut bad = Vec::new();
    toml_suppressions(text, &mut out, |line, code, kind| bad.push((line, String::from_utf8_lossy(code).into_owned(), kind)));
    let lines: Vec<u32> = out.iter().map(|e| e.line).collect();
    assert_eq!(lines, [3, 5, 6, 6], "{out:?}");
    assert!(out[0].selector.matches(602) && !out[0].selector.matches(601));
    assert!(out[1].selector.matches(101) && out[1].selector.matches(604));
    assert!(out[2].selector.matches(601));
    assert_eq!(bad, [(6, "XYZ".to_owned(), BadCodeKind::Malformed), (6, "PM999".to_owned(), BadCodeKind::Unknown)]);
    let _ = Rule(602);
}

#[test]
fn private_components() {
    for name in ["_internal", "_x", "__private", "_helper_"] {
        assert!(is_private(name.as_bytes()), "{name}");
    }
    for name in ["public", "_", "__version__", "__init__", "", "x_"] {
        assert!(!is_private(name.as_bytes()), "{name}");
    }
}

// ---------------------------------------------------------------------------------------------
// `analyse` on small projects with a venv (no third-party code loaded, as `deps --check`)
// ---------------------------------------------------------------------------------------------

const SITE: &str = ".venv/lib/python3.12/site-packages";

/// A throwaway project directory with a venv, removed on drop.
struct Proj(PathBuf);

impl Proj {
    fn new(tag: &str) -> Proj {
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("pymapper-hygiene-{tag}-{}-{n}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("create temp dir");
        let p = Proj(dir.canonicalize().expect("canonicalize"));
        p.file(".venv/pyvenv.cfg", "version = 3.12.4\n");
        p
    }

    fn file(&self, rel: &str, text: &str) -> &Proj {
        let p = self.0.join(rel);
        fs::create_dir_all(p.parent().expect("parent")).expect("mkdir");
        fs::write(&p, text).expect("write");
        self
    }

    /// An installed distribution providing packages `paths` (`pkg`, or `ns/sub` for a
    /// portion of namespace package `ns`) and requiring `requires`.
    fn dist(&self, name: &str, paths: &[&str], requires: &[&str]) -> &Proj {
        let info = format!("{name}-1.0.dist-info");
        let mut record = String::new();
        for path in paths {
            self.file(&format!("{SITE}/{path}/__init__.py"), "");
            record.push_str(&format!("{path}/__init__.py,,\n"));
        }
        record.push_str(&format!("{info}/METADATA,,\n{info}/RECORD,,\n"));
        let mut meta = format!("Metadata-Version: 2.1\nName: {name}\nVersion: 1.0\n");
        for r in requires {
            meta.push_str(&format!("Requires-Dist: {r}\n"));
        }
        self.file(&format!("{SITE}/{info}/METADATA"), &meta).file(&format!("{SITE}/{info}/RECORD"), &record)
    }

    /// Runs every hygiene rule over the first-party files; findings rendered as
    /// `path:line:col PMxxx confidence message`, sorted.
    fn analyse(&self) -> Vec<String> {
        let mut roots = roots::discover(&self.0, &RootOptions::default());
        let (mut files, mut pyprojects) = (PathList::default(), PathList::default());
        collect(&self.0, &mut files, &mut pyprojects);
        let (units, _) = ProjectUnits::discover(&roots.project_root, roots.pyproject.as_ref(), &pyprojects);
        roots.add_unit_roots(&units);
        let ix = load(Inputs::from_roots(&roots, &files));
        let imp = resolve_imports(&ix, &Attribution::from_roots(&roots));
        let mut names = Interner::new();
        let decls = Declarations::load(&units, ix.file_count(), &mut names);
        let cx = HygieneCtx { ix: &ix, imp: &imp, dists: &roots.dists, units: &units, decls: &decls };
        let mut out = Findings::new();
        analyse(&cx, HygieneRules::ALL, &mut names, &mut out);
        let ctx = HygieneContext::new(IndexContext::new(&ix, &self.0, &[]), &names, &decls, &self.0);
        let mut got: Vec<String> = out
            .iter()
            .map(|(f, _)| {
                let mut path = Vec::new();
                ctx.write_path(f.loc.file, &mut path);
                let mut msg = String::new();
                out.write_message(f, &ctx, &mut msg).expect("message");
                format!("{}:{}:{} PM{} {} {msg}", String::from_utf8_lossy(&path), f.loc.line, f.loc.col, f.rule.0, f.confidence)
            })
            .collect();
        got.sort();
        got
    }
}

impl Drop for Proj {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// First-party `.py` and `pyproject.toml` files below `dir` (not the venv), sorted.
fn collect(dir: &Path, files: &mut PathList, pyprojects: &mut PathList) {
    let mut entries: Vec<PathBuf> = fs::read_dir(dir).expect("read dir").map(|e| e.expect("entry").path()).collect();
    entries.sort();
    for p in entries {
        if p.is_dir() {
            if p.file_name() != Some(".venv".as_ref()) {
                collect(&p, files, pyprojects);
            }
        } else if p.extension() == Some("py".as_ref()) {
            files.push(p.as_os_str().as_encoded_bytes()).expect("push");
        } else if p.file_name() == Some("pyproject.toml".as_ref()) {
            pyprojects.push(p.as_os_str().as_encoded_bytes()).expect("push");
        }
    }
}

#[test]
fn analyse_maps_namespace_portions_by_record() {
    let p = Proj::new("ns");
    p.file("pyproject.toml", "[project]\nname = \"app\"\ndependencies = [\"google-cloud-storage\"]\n")
        .file("app/__init__.py", "from google.cloud import storage\nfrom google.cloud import bigquery\nimport google.cloud.storage\n")
        .dist("google-cloud-bigquery", &["google/cloud/bigquery"], &[])
        .dist("google-cloud-storage", &["google/cloud/storage"], &[]);
    assert_eq!(
        p.analyse(),
        ["app/__init__.py:2:6 PM601 100 `google.cloud` is provided by `google-cloud-bigquery`, which is not a declared dependency"]
    );
}

#[test]
fn analyse_meta_packages_and_transitive_dependencies() {
    let p = Proj::new("meta");
    // `bundle` installs no module and requires `core-impl`, which requires `helper`.
    p.file("pyproject.toml", "[project]\nname = \"app\"\ndependencies = [\"bundle\"]\n")
        .file("app/__init__.py", "import core_impl\nimport helper\n")
        .dist("bundle", &[], &["core-impl"])
        .dist("core-impl", &["core_impl"], &["helper>=1"])
        .dist("helper", &["helper"], &[]);
    assert_eq!(
        p.analyse(),
        ["app/__init__.py:2:8 PM604 100 `helper` is provided by `helper`, installed only as a dependency of `bundle`"]
    );
}

#[test]
fn analyse_confidences_by_mapping() {
    let p = Proj::new("conf");
    p.file("pyproject.toml", "[project]\nname = \"app\"\ndependencies = [\"opencv-python-headless\", \"attrs\", \"gone\"]\n")
        .file(
            "app/__init__.py",
            "import cv2\nimport attrs\nimport yaml\nimport unknownpkg\ntry:\n    import orjson\nexcept ImportError:\n    orjson = None\n",
        )
        .dist("orjson", &["orjson"], &[]);
    // Not installed: `cv2` is declared through any OpenCV build, `attrs` by its own name;
    // `yaml` through the name table (90), `unknownpkg` by a guess (60); an import guarded by
    // `except ImportError` is optional (50). `gone` is unused, mapped by a guess.
    assert_eq!(
        p.analyse(),
        [
            "app/__init__.py:3:8 PM601 90 `yaml` is provided by `PyYAML`, which is not a declared dependency",
            "app/__init__.py:4:8 PM601 60 `unknownpkg` is provided by `unknownpkg`, which is not a declared dependency",
            "app/__init__.py:6:12 PM601 50 `orjson` is provided by `orjson`, which is not a declared dependency",
            "pyproject.toml:3:52 PM602 60 dependency `gone` is declared but never imported",
        ]
    );
}

#[test]
fn analyse_workspace_root_groups_and_poetry2() {
    let p = Proj::new("ws");
    p.file(
        "pyproject.toml",
        "[project]\nname = \"root\"\ndependencies = []\n[dependency-groups]\ndev = [\"pytest\"]\n[tool.uv.workspace]\nmembers = [\"libs/*\"]\n",
    )
    .file(
        "libs/a/pyproject.toml",
        "[project]\nname = \"a\"\ndynamic = [\"dependencies\"]\n[tool.poetry.dependencies]\npython = \"^3.10\"\nrequests = \"^2\"\n",
    )
    .file("libs/a/src/a/__init__.py", "import requests\n")
    .file("libs/a/tests/test_a.py", "import pytest\nimport numpy\n")
    .dist("pytest", &["pytest"], &[])
    .dist("requests", &["requests"], &[])
    .dist("numpy", &["numpy"], &[]);
    assert_eq!(
        p.analyse(),
        ["libs/a/tests/test_a.py:2:8 PM601 100 `numpy` is provided by `numpy`, which is not a declared dependency"]
    );
}
