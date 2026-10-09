//! Tests of the evidence and confidence additions: string and config references, quoted
//! types, external overrides, member presets, auto-mode decisions, reasons and wording.

use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use super::tests::{Fx, fx, load_dir};
use super::*;
use crate::config::{Config, Preset};
use crate::extract::config::{ConfigEvidence, config_tokens};
use crate::index::{Inputs, load};
use crate::reach::ModeReason;
use crate::resolve::{Attribution, resolve_calls, resolve_imports};
use crate::roots::{FileStatus, Origin};
use crate::rules::dead_code::{self as dc_rules, reason};

/// One finding: rule, qid (or `qid:name` for PM405), confidence, phrase index, reasons.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Row {
    rule: String,
    qid: String,
    conf: u8,
    phrase: Option<u32>,
    reasons: u32,
}

fn collect(f: &Fx, opts: &DeadCodeOptions<'_>) -> (DeadCode, Vec<Row>) {
    let mut out = Findings::new();
    let dc = run(&f.ix, &f.imp, &f.calls, opts, Emit::ALL, &mut out);
    let rows = out
        .iter()
        .map(|(x, args)| {
            let mut qid = f.ix.qid_string(x.symbol.0);
            let mut phrase = None;
            for a in args {
                match *a {
                    Arg::Phrase(p) => phrase = Some(p),
                    Arg::Name(raw) => {
                        let s = crate::intern::Sym::from_raw(raw).expect("sym");
                        qid = format!("{qid}:{}", String::from_utf8_lossy(f.ix.str(s)));
                    }
                    _ => {}
                }
            }
            Row { rule: x.rule.to_string(), qid, conf: x.confidence, phrase, reasons: x.reasons }
        })
        .collect();
    (dc, rows)
}

fn app_opts<'a>() -> DeadCodeOptions<'a> {
    DeadCodeOptions { mode: Mode::Application, ..DeadCodeOptions::default() }
}

fn find<'r>(rows: &'r [Row], qid: &str) -> Option<&'r Row> {
    rows.iter().find(|r| r.qid == qid)
}

/// A fresh temporary directory.
fn temp_dir(tag: &str) -> PathBuf {
    static N: AtomicU32 = AtomicU32::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("pymapper-dc-ev-{tag}-{}-{n}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

fn write_files(dir: &Path, files: &[(&str, &str)]) {
    for (rel, src) in files {
        let p = dir.join(rel);
        fs::create_dir_all(p.parent().expect("parent")).expect("mkdir");
        fs::write(&p, src).expect("write");
    }
}

/// Dotted module name of a path relative to an import root.
fn module_name(rel: &str) -> String {
    let rel = rel.strip_suffix(".py").unwrap_or(rel);
    let rel = rel.strip_suffix("/__init__").unwrap_or(rel);
    rel.replace('/', ".")
}

/// First-party files plus third-party files (another import root, its own group).
fn fx_mixed(first: &[(&str, &str)], third: &[(&str, &str)]) -> Fx {
    let dir = temp_dir("mixed");
    let (fp, tp) = (dir.join("fp"), dir.join("tp"));
    write_files(&fp, first);
    write_files(&tp, third);
    let mut inp = Inputs::new();
    let g1 = inp.add_group(b"proj", Origin::FirstParty, None);
    let g2 = inp.add_group(b"ext", Origin::ThirdParty, None);
    for (root, files, g) in [(&fp, first, g1), (&tp, third, g2)] {
        for (rel, _) in files {
            let p = root.join(rel);
            inp.push(p.as_os_str().as_bytes(), g, module_name(rel).as_bytes(), FileStatus::Active).expect("push");
        }
    }
    let ix = load(inp);
    let imp = resolve_imports(&ix, &Attribution::new());
    let calls = resolve_calls(&ix, &imp);
    let _ = fs::remove_dir_all(&dir);
    Fx { ix, imp, calls }
}

#[test]
fn string_references_by_dotted_path() {
    let f = fx(&[
        (
            "main.py",
            "X = ['pkg.mod:factory', 'pkg.mod.Klass', 'pkg.plug', 'pkg.mod.not_here', 'pkg.mod:a:b', 'other words']\n\n\
             if __name__ == '__main__':\n    pass\n",
        ),
        ("pkg/__init__.py", ""),
        ("pkg/mod.py", "def factory():\n    helper()\n\ndef helper():\n    pass\n\nclass Klass:\n    pass\n\ndef unrelated():\n    pass\n"),
        ("pkg/plug.py", "def p():\n    pass\n"),
        ("pkg/other.py", "def lonely():\n    pass\n"),
    ]);
    let (dc, rows) = collect(&f, &app_opts());
    let lv = |q: &str| (dc.level(f.node(q)), dc.why(f.node(q)));
    assert_eq!(lv("pkg.mod:factory"), (level::REFERENCED, Why::StringRef));
    assert_eq!(lv("pkg.mod:Klass"), (level::REFERENCED, Why::StringRef));
    // Callees of a referenced def inherit its level.
    assert_eq!(dc.level(f.node("pkg.mod:helper")), level::REFERENCED);
    // `pkg.mod:factory` names the module too; `pkg.plug` is a module path.
    assert_eq!(dc.module_why(f.node("pkg.mod")), Some(Why::StringRef));
    assert_eq!(dc.module_level(f.node("pkg.plug")), Some(level::REFERENCED));
    // Strings that name nothing (or are not paths) keep the rest unused.
    let r = find(&rows, "pkg.mod:unrelated").expect("unrelated reported");
    assert_eq!((r.conf, r.reasons), (100, reason::NO_REFERENCES));
    let m = find(&rows, "pkg.other").expect("module reported");
    assert_eq!((m.rule.as_str(), m.conf, m.phrase), ("PM404", 100, Some(dc_rules::IMP_NEVER)));

    // A file outside any package (not analysed itself) still references by path.
    let f = fx(&[
        ("pkg/__init__.py", ""),
        ("pkg/other.py", "def lonely():\n    pass\n\ndef left():\n    pass\n"),
        ("my-scripts/run.py", "import subprocess\nsubprocess.run(['uvicorn', 'pkg.other:lonely', '--factory'])\n"),
    ]);
    let (dc, rows) = collect(&f, &app_opts());
    assert_eq!(dc.why(f.node("pkg.other:lonely")), Why::StringRef);
    assert_eq!(find(&rows, "pkg.other:left").map(|r| r.conf), Some(100));
}

#[test]
fn quoted_types_use_imports() {
    let f = fx(&[
        ("a.py", "class CursorResult:\n    pass\n\nclass Opt:\n    pass\n\nclass Unused:\n    pass\n"),
        (
            "main.py",
            "from typing import TYPE_CHECKING, cast\n\
             if TYPE_CHECKING:\n    from a import CursorResult, Opt\n    from a import Unused\n\n\
             def f(x) -> 'Opt | None':\n    return cast('CursorResult[int]', x)\n\n\
             if __name__ == '__main__':\n    f(1)\n",
        ),
    ]);
    let (_, rows) = collect(&f, &app_opts());
    let imports: Vec<&Row> = rows.iter().filter(|r| r.rule == "PM405").collect();
    assert_eq!(imports.len(), 1, "{rows:?}");
    assert_eq!(imports[0].qid, "main:Unused");
    assert_eq!(imports[0].conf, 50);
    assert_eq!(imports[0].reasons, reason::TYPE_CHECKING_IMPORT | reason::NO_REFERENCES);
}

const EXT: &[(&str, &str)] = &[(
    "ext/__init__.py",
    "class TypeDecorator:\n    def process_bind_param(self, v, d):\n        return v\n    def other(self):\n        pass\n\n\
     class LLMOp:\n    def _prompts_root(self):\n        return 0\n    def run(self):\n        return self._prompts_root()\n",
)];

const APP_EXT: &[(&str, &str)] = &[(
    "app.py",
    "from ext import TypeDecorator, LLMOp\n\n\
     class JSONType(TypeDecorator):\n    def process_bind_param(self, v, d):\n        return v\n    def helper(self):\n        pass\n\n\
     class _ReportLLMTask:\n    def _prompts_root(self):\n        return 1\n    def unused_mixin(self):\n        pass\n\n\
     class CaseAnalysisTask(_ReportLLMTask, LLMOp):\n    pass\n\n\
     def main():\n    JSONType()\n    CaseAnalysisTask()\n\n\
     if __name__ == '__main__':\n    main()\n",
)];

#[test]
fn overrides_of_external_methods_are_framework_hooks() {
    // Base loaded (`--deps`): overrides of its methods are live with their class.
    let f = fx_mixed(APP_EXT, EXT);
    let (dc, rows) = collect(&f, &app_opts());
    let v = f.node("app:JSONType.process_bind_param");
    assert_eq!((dc.level(v), dc.why(v)), (level::LIVE, Why::ExternalOverride));
    // A mixin before the external base in a subclass's MRO overrides it too.
    let v = f.node("app:_ReportLLMTask._prompts_root");
    assert_eq!((dc.level(v), dc.why(v)), (level::LIVE, Why::ExternalOverride));
    assert_eq!(find(&rows, "app:JSONType.helper").map(|r| r.conf), Some(100));
    assert_eq!(find(&rows, "app:_ReportLLMTask.unused_mixin").map(|r| r.conf), Some(100));

    // Base not loaded: methods of the incomplete class may override something (50).
    let f = fx(APP_EXT);
    let (dc, rows) = collect(&f, &app_opts());
    let v = f.node("app:JSONType.process_bind_param");
    assert_eq!((dc.level(v), dc.why(v)), (level::EXTERNAL_BASE, Why::ExternalBase));
    let r = find(&rows, "app:JSONType.process_bind_param").expect("reported at 50");
    assert_eq!((r.conf, r.reasons, r.phrase), (50, reason::EXTERNAL_BASE, Some(dc_rules::USE_EXTERNAL_BASE)));
    // A base-less mixin of a class with an unloaded base may override it too.
    let v = f.node("app:_ReportLLMTask._prompts_root");
    assert_eq!((dc.level(v), dc.why(v)), (level::EXTERNAL_BASE, Why::ExternalBase));
}

#[test]
fn pydantic_preset_keeps_validators_of_used_models_only() {
    let f = fx(&[(
        "models.py",
        "from pydantic import BaseModel, field_validator, model_validator\n\n\
         class Live(BaseModel):\n    x: int\n    @field_validator('x')\n    def check(cls, v):\n        return v\n    \
         @model_validator(mode='after')\n    def whole(self):\n        return self\n    def model_post_init(self, ctx):\n        pass\n\n\
         class RouteSelection(BaseModel):\n    y: int\n    @field_validator('y')\n    def check_y(cls, v):\n        return v\n\n\
         def main():\n    return Live(x=1)\n\n\
         if __name__ == '__main__':\n    main()\n",
    )]);
    let mut cfg = Config::new(Path::new("/"));
    cfg.presets.push(Preset::Pydantic);
    let m = cfg.entrypoint_matcher();
    let opts = DeadCodeOptions { mode: Mode::Application, ..DeadCodeOptions::from_config(&cfg, &m, None, &[]) };
    let (dc, rows) = collect(&f, &opts);
    for q in ["models:Live.check", "models:Live.whole", "models:Live.model_post_init"] {
        assert_eq!((dc.level(f.node(q)), dc.why(f.node(q))), (level::LIVE, Why::Member), "{q}");
    }
    // The unused model is reported; its validator goes with it.
    let r = find(&rows, "models:RouteSelection").expect("dead model reported");
    assert_eq!((r.rule.as_str(), r.conf), ("PM402", 100));
    assert!(find(&rows, "models:RouteSelection.check_y").is_none(), "{rows:?}");
    assert_eq!(dc.level(f.node("models:RouteSelection.check_y")), 0);
}

#[test]
fn config_files_reference_defs_and_modules() {
    let files: &[(&str, &str)] = &[
        ("pkg/__init__.py", ""),
        ("pkg/tools.py", "def search():\n    pass\n\ndef other():\n    pass\n"),
        ("pkg/app.py", "def create_app():\n    pass\n"),
        ("pkg/x.py", "import pkg.tools, pkg.app\n\ndef lonely_name():\n    pass\n"),
        ("main.py", "import pkg.x\n\nif __name__ == '__main__':\n    pass\n"),
    ];
    let f = fx(files);
    let mut ev = ConfigEvidence::new();
    let mut buf = Vec::new();
    for text in [
        "servers:\n  - module_name: pkg.tools\n    attr_maps:\n      - source_name: search\n",
        "[server]\nfactory = \"pkg.app:create_app\"\n",
        "{\"handler\": \"lonely_name\", \"url\": \"https://example.com/pkg/x\"}\n",
    ] {
        buf.clear();
        config_tokens(text.as_bytes(), &mut buf);
        ev.push_file(&buf);
    }
    let opts = DeadCodeOptions { config: Some(&ev), ..app_opts() };
    let (dc, rows) = collect(&f, &opts);
    let lv = |q: &str| (dc.level(f.node(q)), dc.why(f.node(q)));
    assert_eq!(lv("pkg.app:create_app"), (level::REFERENCED, Why::ConfigRef));
    assert_eq!(lv("pkg.tools:search"), (level::LINKED, Why::ConfigLinked));
    assert_eq!(lv("pkg.x:lonely_name"), (level::EVIDENCE, Why::ConfigName));
    let r = find(&rows, "pkg.x:lonely_name").expect("reported at 50");
    assert_eq!((r.conf, r.reasons, r.phrase), (50, reason::CONFIG_REFERENCE, Some(dc_rules::USE_CONFIG_NAME)));
    let r = find(&rows, "pkg.tools:search").expect("reported at 10");
    assert_eq!((r.conf, r.reasons, r.phrase), (10, reason::CONFIG_REFERENCE, Some(dc_rules::USE_CONFIG_LINKED)));
    assert_eq!(find(&rows, "pkg.tools:other").map(|r| r.conf), Some(100));

    // Off (no config evidence): the same defs look unused.
    let (dc, rows) = collect(&f, &app_opts());
    assert_eq!(dc.level(f.node("pkg.app:create_app")), 0);
    assert_eq!(find(&rows, "pkg.tools:search").map(|r| r.conf), Some(100));
    // Without the name fallback config evidence is not used either.
    let opts = DeadCodeOptions { config: Some(&ev), name_fallback: false, ..app_opts() };
    let (dc, _) = collect(&f, &opts);
    assert_eq!(dc.level(f.node("pkg.app:create_app")), 0);
}

#[test]
fn auto_mode_ignores_helper_guards_and_records_why() {
    let guard = "def run():\n    pass\n\nif __name__ == '__main__':\n    run()\n";
    let base: Vec<(&str, &str)> = vec![
        ("pkg/__init__.py", ""),
        ("pkg/core.py", "def api():\n    pass\n"),
        ("pkg/util/_doctools.py", guard),
        ("pkg/tests/io/generate_legacy.py", guard),
        ("pkg/test_thing.py", guard),
        ("pkg/conftest.py", guard),
        ("pkg/scripts/bump.py", guard),
        ("pkg/benchmarks/bench.py", guard),
    ];
    let dir = temp_dir("auto");
    write_files(&dir, &base);
    let f = load_dir(&dir);
    let units = crate::roots::ProjectUnits::single(&dir, None);
    let modes = crate::reach::FileModes::resolve(&f.ix, Mode::Auto, &units, true);
    assert!(modes.unit_is_library(0));
    assert_eq!(modes.reason(0), ModeReason::NoEntryPoints);
    assert_eq!(modes.ignored_guards(0).0, 6);
    let mut line = String::new();
    modes.describe(&f.ix, &units, &dir, 0, &mut line);
    assert!(line.starts_with("library (no [project.scripts] and no plausible __main__ guard; ignored 6 __main__ guards"), "{line}");
    // Without units (module paths): the same decision.
    let single = crate::reach::FileModes::auto_single(&f.ix, false);
    assert!(single.unit_is_library(0));
    assert_eq!(single.ignored_guards(0).0, 6);
    // Guards stay roots: code under them runs.
    let (dc, _) = f.run_with(&DeadCodeOptions { mode: Mode::Auto, ..DeadCodeOptions::default() });
    assert_eq!(dc.level(f.node("pkg.util._doctools:run")), level::LIVE);

    // A real entry point (a public module's guard, or a package `__main__`) decides.
    write_files(&dir, &[("pkg/cli.py", "import sys\n\nif __name__ == '__main__':\n    sys.exit(0)\n")]);
    let f = load_dir(&dir);
    let modes = crate::reach::FileModes::resolve(&f.ix, Mode::Auto, &units, true);
    assert!(!modes.unit_is_library(0));
    let ModeReason::MainGuard(file, line_no) = modes.reason(0) else { panic!("{:?}", modes.reason(0)) };
    assert!(f.ix.file_path(file).ends_with("pkg/cli.py"));
    assert_eq!(line_no, 3);
    let mut line = String::new();
    modes.describe(&f.ix, &units, &dir, 0, &mut line);
    assert!(line.starts_with("application (__main__ guard at pkg/cli.py:3; ignored 6"), "{line}");
    fs::remove_file(dir.join("pkg/cli.py")).expect("rm");
    write_files(&dir, &[("pkg/__main__.py", "print('hi')\n")]);
    let f = load_dir(&dir);
    let modes = crate::reach::FileModes::resolve(&f.ix, Mode::Auto, &units, true);
    assert!(matches!(modes.reason(0), ModeReason::MainModule(_)), "{:?}", modes.reason(0));
    // A forced mode says so.
    let modes = crate::reach::FileModes::resolve(&f.ix, Mode::Library, &units, true);
    assert_eq!(modes.reason(0), ModeReason::Forced);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn reasons_and_wording_match_the_evidence() {
    let f = fx(&[
        (
            "main.py",
            "import lib\n\n\
             def dead():\n    chained()\n\ndef chained():\n    pass\n\n\
             def run(x=None):\n    x.frobnicate()\n    lib.elsewhere_marker\n\n\
             if __name__ == '__main__':\n    run()\n",
        ),
        (
            "lib.py",
            "class Store:\n    def frobnicate(self):\n        deep()\n\ndef deep():\n    pass\n\n\
             def named():\n    inner()\n\ndef inner():\n    pass\n\nStore\n",
        ),
        ("other.py", "named = 1\n"),
    ]);
    let (_, rows) = collect(&f, &app_opts());
    let get = |q: &str| find(&rows, q).unwrap_or_else(|| panic!("{q} not reported: {rows:?}")).clone();
    // No reference at all: "never used".
    let r = get("main:dead");
    assert_eq!((r.conf, r.phrase, r.reasons), (100, Some(dc_rules::USE_NEVER), reason::NO_REFERENCES));
    // Called only by dead code.
    let r = get("main:chained");
    assert_eq!((r.conf, r.phrase, r.reasons), (100, Some(dc_rules::USE_ONLY_UNUSED), reason::ONLY_USED_BY_UNUSED_CODE));
    // Reached only through a possible call: never "never used".
    let r = get("lib:Store.frobnicate");
    assert_eq!((r.conf, r.phrase, r.reasons), (50, Some(dc_rules::USE_POSSIBLE), reason::POSSIBLE_CALL_ONLY));
    // Its callee inherits the level: "only used by code that may be unused".
    let r = get("lib:deep");
    assert_eq!(r.conf, 50);
    assert_eq!(r.phrase, Some(dc_rules::USE_MAYBE_UNUSED));
    assert_eq!(r.reasons, reason::POSSIBLE_CALL_ONLY | reason::ONLY_USED_BY_UNUSED_CODE);
    // The name occurs in another (unrelated) file.
    let r = get("lib:named");
    assert_eq!((r.conf, r.phrase, r.reasons), (50, Some(dc_rules::USE_NAME_ELSEWHERE), reason::NAME_ELSEWHERE));
    // Every finding carries reasons; "never used" only with `no-references`.
    for r in &rows {
        assert_ne!(r.reasons, 0, "{r:?}");
        if r.phrase == Some(dc_rules::USE_NEVER) {
            assert_eq!(r.reasons & reason::NO_REFERENCES, reason::NO_REFERENCES, "{r:?}");
        }
    }
}
