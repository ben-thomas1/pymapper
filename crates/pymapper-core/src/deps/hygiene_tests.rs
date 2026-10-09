//! Tests of the dependency-hygiene building blocks: `pyproject.toml` declarations, requirement
//! names, TOML comment suppressions and private names. End-to-end behaviour (mapping, findings,
//! output) is covered by `crates/pymapper-cli/tests/deps_check.rs`.

use std::path::PathBuf;

use super::declared::{DeclStatus, Declarations, DepKind, parse_requirement, toml_suppressions};
use super::hygiene::is_private;
use crate::index::NONE;
use crate::intern::Interner;
use crate::rules::{BadCodeKind, FileId, Rule};

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
