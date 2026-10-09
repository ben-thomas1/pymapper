//! Tests of name evidence from test code and from files that are not importable: tests in
//! hyphenated directories (`tests/my-service/pkg/test_x.py`), shadowed dotted paths, test
//! helpers reached through `sys.path` tricks, `_testing` packages, and the analysis of
//! non-importable files themselves.

use std::os::unix::ffi::OsStrExt;

use super::evidence_tests::{app_opts, collect, find};
use super::tests::{Fx, fx};
use super::*;
use crate::index::{Inputs, load};
use crate::resolve::{Attribution, resolve_calls, resolve_imports};
use crate::roots::FileStatus;
use crate::rules::dead_code::{self as dc_rules, reason};

/// `pkg.types:Registry` is live (built by `main`); its members are read in various files.
const REGISTRY: &[(&str, &str)] = &[
    ("pkg/__init__.py", ""),
    (
        "pkg/types.py",
        "class Registry:\n\
         \x20   @property\n    def reclaim_reserved(self):\n        return 0\n\n\
         \x20   @property\n    def kernel_keys(self):\n        return []\n\n\
         \x20   def used_in_prod(self):\n        pass\n\n\
         \x20   def dead_method(self):\n        pass\n",
    ),
    ("main.py", "from pkg.types import Registry\n\nif __name__ == '__main__':\n    r = Registry()\n    r.used_in_prod()\n"),
];

fn with(base: &[(&'static str, &'static str)], more: &[(&'static str, &'static str)]) -> Fx {
    let mut files = base.to_vec();
    files.extend_from_slice(more);
    fx(&files)
}

#[test]
fn hyphenated_test_dirs_give_test_only_evidence() {
    let f = with(
        REGISTRY,
        &[
            // Not importable (`my-service`), like `tests/cogito-forge/cogito/forge/...`.
            ("tests/my-service/pkg/__init__.py", ""),
            (
                "tests/my-service/pkg/test_queue.py",
                "from pkg.types import Registry\n\n\
                 def test_reclaim(registry):\n    assert registry.reclaim_reserved == 0\n\n\
                 def _helper_used():\n    return 1\n\n\
                 def test_helper():\n    assert _helper_used() == 1\n\n\
                 def _unused_helper():\n    pass\n",
            ),
            // No import of the module: the name alone.
            ("tests/my-service/pkg/test_server.py", "def test_keys(server):\n    assert server.kernel_keys == []\n"),
        ],
    );
    let tq = "tests.my-service.pkg.test_queue";
    assert_eq!(f.ix.file_status(f.ix.file(f.node(tq)).expect("file")), FileStatus::NotImportable);
    let (dc, rows) = collect(&f, &app_opts());
    let get = |q: &str| find(&rows, q).unwrap_or_else(|| panic!("{q} not reported: {rows:?}")).clone();
    // Read by a test file importing its module: linked, below the gate, named as test-only.
    let r = get("pkg.types:Registry.reclaim_reserved");
    assert_eq!((r.conf, r.phrase, r.reasons), (10, Some(dc_rules::USE_TESTS), reason::USED_ONLY_IN_TESTS));
    assert_eq!(dc.why(f.node("pkg.types:Registry.reclaim_reserved")), Why::TestLinked);
    // Read by a test file that does not import it: name evidence.
    let r = get("pkg.types:Registry.kernel_keys");
    assert_eq!((r.conf, r.phrase, r.reasons), (50, Some(dc_rules::USE_TESTS), reason::USED_ONLY_IN_TESTS));
    assert_eq!(dc.why(f.node("pkg.types:Registry.kernel_keys")), Why::TestEvidence);
    // Nothing mentions it anywhere.
    let r = get("pkg.types:Registry.dead_method");
    assert_eq!((r.conf, r.reasons), (100, reason::NO_REFERENCES));
    // The test files are analysed: their unused helpers are reported, their tests and the
    // helpers the tests call are not, and a module that cannot be imported has no PM404.
    let r = get(&format!("{tq}:_unused_helper"));
    assert_eq!((r.rule.as_str(), r.conf), ("PM401", 100));
    for q in [format!("{tq}:test_reclaim"), format!("{tq}:_helper_used"), tq.to_string(), "tests.my-service.pkg.test_server".into()] {
        assert!(find(&rows, &q).is_none(), "{q} reported: {rows:?}");
    }
}

#[test]
fn production_evidence_wins_over_test_evidence() {
    let f = with(
        REGISTRY,
        &[
            // Mentioned by production code that does not import it, and by a test that does.
            ("pkg/report.py", "def show(x):\n    return x.reclaim_reserved\n"),
            ("tests/my-service/test_a.py", "import pkg.types\n\ndef test_a(r):\n    assert r.reclaim_reserved\n    assert r.kernel_keys\n"),
            // Mentioned by a production file importing it, and by a test elsewhere.
            ("pkg/view.py", "import pkg.types\n\ndef render(x):\n    return x.kernel_keys\n"),
        ],
    );
    let (dc, rows) = collect(&f, &app_opts());
    // The test link is the strongest evidence, but production code mentions the name too: not
    // "only in tests".
    let v = f.node("pkg.types:Registry.reclaim_reserved");
    assert_eq!((dc.level(v), dc.why(v)), (level::LINKED, Why::Linked));
    let r = find(&rows, "pkg.types:Registry.reclaim_reserved").expect("reported");
    assert_eq!(r.reasons & reason::USED_ONLY_IN_TESTS, 0, "{r:?}");
    let v = f.node("pkg.types:Registry.kernel_keys");
    assert_eq!((dc.level(v), dc.why(v)), (level::LINKED, Why::Linked));
}

#[test]
fn importable_tests_and_testing_packages_are_test_code() {
    let f = with(
        REGISTRY,
        &[
            ("tests/__init__.py", ""),
            ("tests/test_types.py", "from pkg.types import Registry\n\ndef test_r(r):\n    assert r.reclaim_reserved == 0\n"),
            // A test-support package shipped in the library (`pandas/_testing`).
            ("pkg/_testing/__init__.py", "def assert_keys(r):\n    assert r.kernel_keys == []\n"),
        ],
    );
    let (dc, rows) = collect(&f, &app_opts());
    assert_eq!(dc.why(f.node("pkg.types:Registry.reclaim_reserved")), Why::TestLinked);
    assert_eq!(dc.why(f.node("pkg.types:Registry.kernel_keys")), Why::TestEvidence);
    // `_testing` code is still analysed for dead code.
    assert_eq!(find(&rows, "pkg._testing:assert_keys").map(|r| r.conf), Some(100), "{rows:?}");
}

#[test]
fn tests_importing_through_sys_path_tricks() {
    let f = fx(&[
        // `tools` is not a package of the project; the test puts it on `sys.path` by hand.
        ("tools/helper_mod.py", "def only_from_tests():\n    pass\n\ndef never():\n    pass\n"),
        (
            "tests/e2e-suite/test_tools.py",
            "import os\nimport sys\n\
             sys.path.insert(0, os.path.join(os.path.dirname(__file__), '..', '..', 'tools'))\n\
             import helper_mod  # noqa: E402\n\n\
             def test_it():\n    helper_mod.only_from_tests()\n",
        ),
    ]);
    let (dc, rows) = collect(&f, &app_opts());
    // The import does not resolve, so the use is name evidence from test code only.
    let v = f.node("tools.helper_mod:only_from_tests");
    assert_eq!((dc.level(v), dc.why(v)), (level::EVIDENCE, Why::TestEvidence));
    let r = find(&rows, "tools.helper_mod:only_from_tests").expect("reported");
    assert_eq!((r.conf, r.reasons), (50, reason::USED_ONLY_IN_TESTS));
    assert_eq!(find(&rows, "tools.helper_mod:never").map(|r| r.conf), Some(100));
}

#[test]
fn module_names_only_in_tests() {
    let f = fx(&[
        ("pkg/__init__.py", ""),
        ("pkg/plugin.py", "def hook():\n    pass\n"),
        ("pkg/lonely.py", "def x():\n    pass\n"),
        // Imported only by a non-importable test: the test module is a root, so it is live.
        ("pkg/tested.py", "def t():\n    pass\n"),
        ("main.py", "if __name__ == '__main__':\n    pass\n"),
        (
            "tests/my-service/test_plugins.py",
            "import pkg.tested\n\ndef test_load(loader):\n    loader.load('plugin')\n    pkg.tested.t()\n",
        ),
    ]);
    let (dc, rows) = collect(&f, &app_opts());
    let m = find(&rows, "pkg.plugin").expect("plugin reported");
    assert_eq!((m.rule.as_str(), m.conf, m.phrase, m.reasons), ("PM404", 50, Some(dc_rules::IMP_TESTS), reason::USED_ONLY_IN_TESTS));
    assert_eq!(dc.module_why(f.node("pkg.plugin")), Some(Why::TestEvidence));
    assert_eq!(find(&rows, "pkg.lonely").map(|r| r.conf), Some(100));
    assert!(find(&rows, "pkg.tested").is_none(), "{rows:?}");
    assert!(find(&rows, "pkg.tested:t").is_none(), "{rows:?}");
}

#[test]
fn non_importable_scripts_are_analysed() {
    let f = fx(&[
        ("pkg/__init__.py", ""),
        ("pkg/core.py", "def used_by_script():\n    pass\n"),
        (
            "scripts/bump-version.py",
            "from pkg.core import used_by_script\n\n\
             def main():\n    used_by_script()\n\ndef unused():\n    pass\n\n\
             if __name__ == '__main__':\n    main()\n",
        ),
    ]);
    let (_, rows) = collect(&f, &app_opts());
    assert_eq!(find(&rows, "scripts.bump-version:unused").map(|r| r.conf), Some(100), "{rows:?}");
    for q in ["scripts.bump-version", "scripts.bump-version:main", "pkg.core:used_by_script", "pkg.core"] {
        assert!(find(&rows, q).is_none(), "{q} reported: {rows:?}");
    }
}

/// Loads `(rel, module, status, source)` files as one first-party group.
fn fx_status(tag: &str, files: &[(&str, &str, FileStatus, &str)]) -> Fx {
    let dir = std::env::temp_dir().join(format!("pymapper-dc-tc-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let mut inp = Inputs::new();
    let g = inp.add_group(b"proj", Origin::FirstParty, None);
    for &(rel, module, status, src) in files {
        let p = dir.join(rel);
        std::fs::create_dir_all(p.parent().expect("parent")).expect("mkdir");
        std::fs::write(&p, src).expect("write");
        inp.push(p.as_os_str().as_bytes(), g, module.as_bytes(), status).expect("push");
    }
    let ix = load(inp);
    let imp = resolve_imports(&ix, &Attribution::new());
    let calls = resolve_calls(&ix, &imp);
    let _ = std::fs::remove_dir_all(&dir);
    Fx { ix, imp, calls }
}

#[test]
fn shadowed_dotted_paths_still_give_evidence() {
    use FileStatus::{Active, Shadowed};
    // A test tree put on `sys.path` (`tests/svc`) repeats the dotted paths of the real package:
    // its copies are shadowed by the real modules, so they are not analysed, but what they read
    // still counts.
    let f = fx_status(
        "shadow",
        &[
            ("src/pkg/__init__.py", "pkg", Active, ""),
            (
                "src/pkg/types.py",
                "pkg.types",
                Active,
                "def read_by_fixture():\n    pass\n\ndef read_by_helper():\n    pass\n\ndef never():\n    pass\n",
            ),
            ("src/main.py", "main", Active, "import pkg.types\n\nif __name__ == '__main__':\n    pass\n"),
            ("tests/svc/pkg/__init__.py", "pkg", Shadowed, ""),
            ("tests/svc/pkg/conftest.py", "pkg.conftest", Active, "import pkg.types\n\ndef fx():\n    return pkg.types.read_by_fixture\n"),
            // Same dotted path as the real module: shadowed, not test-named.
            (
                "tests/svc/pkg/types.py",
                "pkg.types",
                Shadowed,
                "def helper(m):\n    return m.read_by_helper\n\ndef shadow_only():\n    pass\n",
            ),
        ],
    );
    let (dc, rows) = collect(&f, &app_opts());
    let v = f.node("pkg.types:read_by_fixture");
    assert_eq!((dc.level(v), dc.why(v)), (level::LINKED, Why::TestLinked));
    let v = f.node("pkg.types:read_by_helper");
    assert_eq!((dc.level(v), dc.why(v)), (level::EVIDENCE, Why::Evidence));
    assert_eq!(find(&rows, "pkg.types:never").map(|r| r.conf), Some(100), "{rows:?}");
    // The shadowed copy keeps its nodes but is not analysed: nothing is reported in it.
    let shadow = (0..f.ix.file_count()).find(|&i| f.ix.file_path(i).ends_with("svc/pkg/types.py")).expect("shadowed file");
    assert_eq!(f.ix.file_status(shadow), Shadowed);
    assert_ne!(f.ix.file_node(shadow), crate::index::NONE);
    assert!(rows.iter().all(|r| !r.qid.ends_with(":shadow_only") && !r.qid.ends_with(":helper")), "{rows:?}");
}
