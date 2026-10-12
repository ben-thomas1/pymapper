//! Tests for config parsing, validation, discovery, overrides and the template.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use super::parse::TABLES;
use super::*;

fn base() -> PathBuf {
    PathBuf::from("/proj")
}

fn ok(text: &str) -> Config {
    parse_standalone(text, "pymapper.toml", &base()).unwrap_or_else(|e| panic!("{e}"))
}

fn errs(text: &str) -> Vec<Diagnostic> {
    match parse_standalone(text, "pymapper.toml", &base()) {
        Ok(_) => panic!("expected errors for:\n{text}"),
        Err(ConfigError::Invalid { diagnostics }) => diagnostics,
        Err(e) => panic!("unexpected error {e}"),
    }
}

fn one_err(text: &str) -> Diagnostic {
    let mut d = errs(text);
    assert_eq!(d.len(), 1, "{d:#?}");
    d.pop().unwrap()
}

// ----- template -----

/// Uncomments `# key = value` lines (default values) whose key is known for the current section.
fn uncomment_defaults(t: &str) -> String {
    let mut section = "";
    let mut out = String::new();
    for line in t.lines() {
        if let Some(h) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            section = TABLES.iter().find(|(name, _)| *name == h).map_or("?", |(name, _)| name);
        }
        let uncommented = line.strip_prefix("# ").filter(|rest| {
            rest.split_once(" = ").is_some_and(|(k, _)| {
                TABLES.iter().any(|(name, keys)| *name == section && keys.contains(&k))
            })
        });
        out.push_str(uncommented.unwrap_or(line));
        out.push('\n');
    }
    out
}

/// Uncomments the example lines (`#   key = value`, `#   [table]`, `#   "glob" = [...]`).
fn uncomment_examples(t: &str) -> String {
    let mut out = String::new();
    for line in t.lines() {
        let ex = line.strip_prefix("#   ").filter(|rest| {
            rest.starts_with('[')
                || rest.starts_with('"') && rest.contains("\" = [")
                || rest.split_once(" = ").is_some_and(|(k, _)| TABLES.iter().any(|(_, keys)| keys.contains(&k)))
        });
        out.push_str(ex.unwrap_or(line));
        out.push('\n');
    }
    out
}

#[test]
fn template_as_shipped_is_the_defaults() {
    assert_eq!(ok(TEMPLATE), Config::new(&base()));
}

#[test]
fn template_documented_defaults_match_code() {
    let text = uncomment_defaults(TEMPLATE);
    // Guard against the uncommenting silently matching nothing.
    let active = text.lines().filter(|l| l.contains(" = ") && !l.starts_with('#')).count();
    assert!(active >= 30, "only {active} default lines uncommented");
    assert_eq!(ok(&text), Config::new(&base()));
}

#[test]
fn template_documents_every_option() {
    for (table, keys) in TABLES {
        for key in *keys {
            let documented = [
                format!("# {key} ="),
                format!("#   {key} ="),
                format!("#   {key} "),
                format!("\n[{key}]\n"),
                format!("[[{key}]]"),
            ]
                .iter()
                .any(|pat| TEMPLATE.contains(pat.as_str()));
            assert!(documented, "option `{table}.{key}` is not documented in pymapper.toml.template");
        }
    }
    for &p in Preset::ALL {
        assert!(TEMPLATE.contains(&format!("\"{p}\"")), "preset {p} not documented");
    }
    for name in OutputFormat::NAMES.iter().chain(Mode::NAMES).chain(ContractKindName::NAMES).chain(GroupBy::NAMES).chain(ColorChoice::NAMES) {
        assert!(TEMPLATE.contains(&format!("\"{name}\"")), "value {name} not documented");
    }
    // Every option carries a cost note in its section.
    assert!(TEMPLATE.matches("cost:").count() >= 25);
}

#[test]
fn template_examples_are_valid() {
    let cfg = ok(&uncomment_examples(TEMPLATE));
    assert_eq!(cfg.paths, vec![PathBuf::from("/proj/src"), PathBuf::from("/proj/tests")]);
    assert_eq!(cfg.python_version, Some(PythonVersion { major: 3, minor: 13 }));
    assert_eq!(cfg.presets, vec![Preset::Fastapi, Preset::Pydantic, Preset::Pytest]);
    assert_eq!(cfg.rules.per_file_ignores.len(), 2);
    assert_eq!(cfg.contracts.len(), 3);
    assert_eq!(cfg.analysis.coverage, Some(PathBuf::from("/proj/coverage.json")));
}

// ----- values -----

#[test]
fn full_document() {
    let cfg = ok(r#"
paths = ["src", "./tests/../tests"]
src = ["src"]
exclude = ["a"]
extend-exclude = ["b"]
respect-gitignore = false
include-deps = true
include-stdlib = true
venv = "../venv"
python-version = "3.12"
cache = false
cache-dir = "/tmp/c"
threads = 8
mode = "library"
output-format = "sarif"
group-by = "file"
color = "never"
limit = 0
min-confidence = 60
baseline = "base.json"
presets = ["flask", "pytest", "flask"]

[rules]
select = ["ALL"]
extend-select = ["PM9"]
ignore = ["PM101", "PM2"]
per-file-ignores = { "tests/**" = ["PM3"], "a.py" = [] }

[thresholds]
max-mccabe = 12
max-cognitive = 0
max-nesting = 3
max-function-loc = 50
max-params = 4
min-maintainability = 20
max-module-loc = 500
max-fan-out = 10
dupes-min-tokens = 80
dupes-min-lines = 8

[entrypoints]
decorators = ["*.task"]
bases = ["x.Base"]
names = ["main", "pkg.mod:run"]
project-scripts = false

[analysis]
dupes = true
exact-reachability = true
name-fallback = false
unique-method-fallback = false
type-checking-cycles = true
coverage = "cov.json"
"#);
    assert_eq!(cfg.paths, vec![PathBuf::from("/proj/src"), PathBuf::from("/proj/tests")]);
    assert_eq!(cfg.src, vec![PathBuf::from("/proj/src")]);
    assert_eq!(cfg.all_excludes().collect::<Vec<_>>(), ["a", "b"]);
    assert!(!cfg.respect_gitignore && cfg.include_deps && cfg.include_stdlib && !cfg.cache);
    assert_eq!(cfg.venv, Some(PathBuf::from("/venv")));
    assert_eq!(cfg.python_version.unwrap().to_string(), "3.12");
    assert_eq!(cfg.cache_dir, PathBuf::from("/tmp/c"));
    assert_eq!(cfg.threads, 8);
    assert_eq!(cfg.mode, Mode::Library);
    assert_eq!(cfg.output_format, OutputFormat::Sarif);
    assert_eq!((cfg.group_by, cfg.color, cfg.limit), (GroupBy::File, ColorChoice::Never, 0));
    assert_eq!(cfg.min_confidence, 60);
    assert_eq!(cfg.baseline, Some(PathBuf::from("/proj/base.json")));
    assert_eq!(cfg.presets, vec![Preset::Flask, Preset::Pytest]);
    assert_eq!(cfg.rules.select, Some(vec![RuleSelector::All]));
    assert_eq!(cfg.rules.ignore.len(), 2);
    assert_eq!(cfg.rules.per_file_ignores[0].pattern, "a.py");
    assert!(cfg.rules.per_file_ignores[1].ignores(301));
    assert!(!cfg.rules.per_file_ignores[1].ignores(201));
    assert_eq!(cfg.thresholds.max_mccabe, 12);
    assert_eq!(cfg.thresholds.max_cognitive, 0);
    assert_eq!(cfg.thresholds.min_maintainability, 20.0);
    assert_eq!(cfg.thresholds.dupes_min_lines, 8);
    assert!(!cfg.entrypoints.project_scripts);
    assert!(cfg.analysis.dupes && cfg.analysis.exact_reachability && !cfg.analysis.name_fallback);
    assert!(!cfg.analysis.unique_method_fallback && cfg.analysis.type_checking_cycles);
    assert_eq!(cfg.analysis.coverage, Some(PathBuf::from("/proj/cov.json")));
}

#[test]
fn round_trip_through_to_toml() {
    let mut cfg = ok(&uncomment_examples(TEMPLATE));
    cfg.venv = Some(PathBuf::from("/odd \"path\"\\x"));
    cfg.thresholds.min_maintainability = 12.5;
    cfg.rules.extend_select = vec![RuleSelector::Prefix { value: 1, digits: 2 }];
    let text = cfg.to_toml();
    assert_eq!(ok(&text), cfg, "\n{text}");
    let defaults = Config::new(&base());
    assert_eq!(ok(&defaults.to_toml()), defaults);
}

// ----- strict validation -----

#[test]
fn unknown_key_with_suggestion_and_position() {
    let d = one_err("threads = 2\nexlcude = []\n");
    assert_eq!((d.line, d.col), (2, 1));
    assert_eq!(d.message, "unknown option `exlcude`");
    assert_eq!(d.help.as_deref(), Some("did you mean `exclude`?"));
    assert_eq!(d.to_string(), "pymapper.toml:2:1: unknown option `exlcude`\n  help: did you mean `exclude`?");

    let d = one_err("[rules]\nselct = []\n");
    assert_eq!(d.help.as_deref(), Some("did you mean `rules.select`?"));
    let d = one_err("[thresholds]\nmax_mccabe = 3\n");
    assert_eq!(d.help.as_deref(), Some("did you mean `thresholds.max-mccabe`?"));
    let d = one_err("[analysis]\nfoo = 1\n");
    assert_eq!(d.message, "unknown option `analysis.foo`");
    assert_eq!(d.help, None);
}

#[test]
fn tool_pymapper_header_in_standalone_file_has_hint() {
    let d = one_err("[tool.pymapper]\nthreads = 2\n");
    assert_eq!(d.message, "unknown option `tool`");
    assert!(d.help.as_deref().is_some_and(|h| h.contains("remove the `[tool.pymapper]` header")), "{d}");
}

#[test]
fn key_in_wrong_table_hint() {
    let d = one_err("select = [\"PM1\"]\n");
    assert_eq!(d.help.as_deref(), Some("`select` belongs in `[rules]`"));
    let d = one_err("[rules]\nthreads = 3\n");
    assert_eq!(d.help.as_deref(), Some("`threads` belongs in the top-level table"));
}

#[test]
fn type_and_range_errors() {
    let cases: &[(&str, &str)] = &[
        ("threads = \"4\"", "`threads`: expected an integer, found a string"),
        ("threads = -1", "`threads`: -1 is out of range (0..=4096)"),
        ("min-confidence = 101", "`min-confidence`: 101 is out of range (0..=100)"),
        ("cache = 1", "`cache`: expected a boolean (`true` or `false`), found an integer"),
        ("paths = \"src\"", "`paths`: expected an array of paths, found a string"),
        ("paths = []", "`paths`: must list at least one path"),
        ("exclude = [\"\"]", "`exclude`: must not be empty"),
        ("rules = 3", "`rules`: expected a table, found an integer"),
        ("[thresholds]\nmax-mccabe = 0", "`thresholds.max-mccabe`: 0 is out of range (1..=2147483647)"),
        ("[thresholds]\nmin-maintainability = 101.5", "`thresholds.min-maintainability`: value is out of range (0..=100)"),
        ("[thresholds]\nmin-maintainability = true", "`thresholds.min-maintainability`: expected a number, found a boolean"),
        ("python-version = \"2.7\"", "`python-version`: unsupported Python version `2.7`: only Python 3 is supported"),
        ("python-version = \"3\"", "`python-version`: invalid Python version `3` (expected `3.N`, e.g. `3.13`)"),
        ("[entrypoints]\ndecorators = [\"a b\"]", "`entrypoints.decorators`: invalid pattern `a b`: unsupported character ' ' in pattern (only `*` and `?` are wildcards)"),
        ("[entrypoints]\nnames = [\"\"]", "`entrypoints.names`: invalid pattern ``: empty pattern"),
        ("venv = \"\"", "`venv`: must not be empty"),
        ("contracts = {}", "`contracts`: expected an array of tables (`[[contracts]]`), found a table"),
    ];
    for (text, msg) in cases {
        assert_eq!(one_err(text).message, *msg, "for {text:?}");
    }
}

#[test]
fn enum_values_suggest() {
    let d = one_err("output-format = \"jsn\"");
    assert_eq!(d.message, "`output-format`: unknown output format `jsn` (expected one of: human, llm, concise, json, sarif, github)");
    assert_eq!(d.help.as_deref(), Some("did you mean `json`?"));
    let d = one_err("presets = [\"flask\", \"djang\"]");
    assert_eq!(d.help.as_deref(), Some("did you mean `django`?"));
    assert_eq!((d.line, d.col), (1, 21));
    let d = one_err("mode = \"lib\"");
    assert_eq!(d.help, None);
    let d = one_err("[rules]\nignore = [\"pm101\"]");
    assert_eq!(d.help.as_deref(), Some("did you mean `PM101`?"));
    let d = one_err("[rules]\nselect = [\"PM1000\"]");
    assert!(d.message.contains("invalid rule selector `PM1000`"));
}

#[test]
fn all_errors_reported_in_source_order() {
    let d = errs("zzz = 1\nthreads = true\n[rules]\nselect = [\"X\", 3]\n[thresholds]\nmax-nesting = \"deep\"\n");
    let lines: Vec<(u32, u32)> = d.iter().map(|d| (d.line, d.col)).collect();
    assert_eq!(lines, [(1, 1), (2, 11), (4, 11), (4, 16), (6, 15)], "{d:#?}");
    let err = ConfigError::Invalid { diagnostics: d };
    let text = err.to_string();
    assert!(text.starts_with("invalid configuration:\npymapper.toml:1:1: unknown option `zzz`"), "{text}");
}

#[test]
fn toml_syntax_error_is_located() {
    let d = one_err("threads = 4\nmode = \n");
    assert_eq!(d.line, 2);
    assert!(d.message.starts_with("invalid TOML:"), "{}", d.message);
    let d = one_err("threads = 1\nthreads = 2\n");
    assert!(d.message.contains("invalid TOML"), "{}", d.message);
}

#[test]
fn utf8_columns_count_characters() {
    let d = one_err("# é\nmode = \"é\"\n");
    assert_eq!((d.line, d.col), (2, 8));
}

// ----- contracts -----

#[test]
fn contracts_parse() {
    let cfg = ok(r#"
[[contracts]]
name = "layers"
kind = "layers"
layers = ["a.api", ["a.x", "a.y"], "a.db"]
containers = ["app1", "app2"]
independent-siblings = true
ignore-imports = ["a.db.* -> a.api", "a.**->b"]
allow-type-checking = false
transitive = true

[[contracts]]
name = "forbid"
kind = "forbidden"
source = ["a.core"]
forbidden = ["requests"]

[[contracts]]
name = "indep"
kind = "independence"
modules = ["p.a", "p.b"]
"#);
    let c = &cfg.contracts[0];
    assert!(!c.allow_type_checking && c.transitive);
    assert_eq!(c.ignore_imports[1], ImportPattern { importer: "a.**".into(), imported: "b".into() });
    let ContractKind::Layers { layers, containers, independent_siblings } = &c.kind else { panic!() };
    assert_eq!(layers.len(), 3);
    assert_eq!(layers[1], ["a.x", "a.y"]);
    assert_eq!(containers.len(), 2);
    assert!(*independent_siblings);
    assert_eq!(cfg.contracts[1].kind.name(), ContractKindName::Forbidden);
    assert!(cfg.contracts[2].allow_type_checking && !cfg.contracts[2].transitive);
}

#[test]
fn contract_errors() {
    let cases: &[(&str, &str, Option<&str>)] = &[
        ("[[contracts]]\nkind = \"layers\"\nlayers = [\"a\", \"b\"]", "contract is missing `name`", None),
        ("[[contracts]]\nname = \"c\"\nlayers = [\"a\", \"b\"]", "contract `c` is missing `kind`", Some("add `kind = ` one of: layers, forbidden, independence")),
        ("[[contracts]]\nname = \"c\"\nkind = \"layer\"\nlayers = [\"a\", \"b\"]", "`contracts.kind`: unknown contract kind `layer` (expected one of: layers, forbidden, independence)", Some("did you mean `layers`?")),
        ("[[contracts]]\nname = \"c\"\nkind = \"layers\"", "contract `c` (kind `layers`) is missing `layers`", None),
        ("[[contracts]]\nname = \"c\"\nkind = \"layers\"\nlayers = [\"a\"]", "contract `c`: `layers` needs at least 2 layers (found 1)", Some("with fewer than 2 layers there is nothing to check")),
        ("[[contracts]]\nname = \"c\"\nkind = \"layers\"\nlayers = []", "contract `c`: `layers` needs at least 2 layers (found 0)", Some("with fewer than 2 layers there is nothing to check")),
        ("[[contracts]]\nname = \"c\"\nkind = \"layers\"\nlayers = [\"a\", []]", "`contracts.layers`: a layer must name at least one module", None),
        ("[[contracts]]\nname = \"c\"\nkind = \"layers\"\nlayers = [\"a\", 1]", "`contracts.layers`: expected a module name or an array of sibling module names, found an integer", None),
        ("[[contracts]]\nname = \"c\"\nkind = \"layers\"\nlayers = [\"a\", \"b\"]\nmodules = [\"x\"]", "`contracts.modules` is not valid for contracts of kind `layers`", Some("`modules` belongs to kind `independence`")),
        ("[[contracts]]\nname = \"c\"\nkind = \"layers\"\nlayers = [\"a\", \"b\"]\nlayer = 1", "unknown option `contracts.layer`", Some("did you mean `contracts.layers`?")),
        ("[[contracts]]\nname = \"c\"\nkind = \"layers\"\nlayers = [\"a\", [\"b\", \"a\"]]", "contract `c`: module `a` is listed more than once", None),
        ("[[contracts]]\nname = \"c\"\nkind = \"layers\"\nlayers = [\"a\", \"b-c\"]", "`contracts.layers`: invalid module name `b-c`", Some("use dotted names like `pkg.sub` (wildcards are only allowed in `ignore-imports`)")),
        ("[[contracts]]\nname = \"c\"\nkind = \"layers\"\nlayers = [\"a\", \"b.*\"]", "`contracts.layers`: invalid module name `b.*`", Some("use dotted names like `pkg.sub` (wildcards are only allowed in `ignore-imports`)")),
        ("[[contracts]]\nname = \"c\"\nkind = \"forbidden\"\nsource = [\"a\"]", "contract `c` (kind `forbidden`) is missing `forbidden`", None),
        ("[[contracts]]\nname = \"c\"\nkind = \"independence\"\nmodules = [\"a\"]", "contract `c`: `modules` needs at least 2 modules (found 1)", Some("with fewer than 2 modules there is nothing to check")),
        ("[[contracts]]\nname = \"c\"\nkind = \"independence\"\nmodules = []", "contract `c`: `modules` needs at least 2 modules (found 0)", Some("with fewer than 2 modules there is nothing to check")),
        ("[[contracts]]\nname = \"c\"\nkind = \"forbidden\"\nsource = []\nforbidden = [\"b\"]", "contract `c`: `source` must not be empty", Some("an empty list matches no imports, so the contract could never fail")),
        ("[[contracts]]\nname = \"c\"\nkind = \"forbidden\"\nsource = [\"a\"]\nforbidden = []", "contract `c`: `forbidden` must not be empty", Some("an empty list matches no imports, so the contract could never fail")),
        ("[[contracts]]\nname = \"c\"\nkind = \"independence\"\nmodules = [\"a\", \"b\"]\nignore-imports = [\"a b\"]", "`contracts.ignore-imports`: expected `importer -> imported`, found `a b`", Some("write `importer -> imported`, e.g. `pkg.api -> pkg.db.*`")),
        ("[[contracts]]\nname = \"c\"\nkind = \"independence\"\nmodules = [\"a\", \"b\"]\nignore-imports = [\"a -> \"]", "`contracts.ignore-imports`: invalid module pattern `` in `a -> `", Some("use dotted names like `pkg.sub`; `*` matches one segment, `**` any number")),
        ("[[contracts]]\nname = \"  \"\nkind = \"independence\"\nmodules = [\"a\", \"b\"]", "`contracts.name`: must not be empty", None),
    ];
    for (text, msg, help) in cases {
        let d = one_err(text);
        assert_eq!(d.message, *msg, "for {text:?}");
        assert_eq!(d.help.as_deref(), *help, "for {text:?}");
    }
}

#[test]
fn duplicate_contract_names() {
    let d = one_err("[[contracts]]\nname = \"c\"\nkind = \"independence\"\nmodules = [\"a\", \"b\"]\n\n[[contracts]]\nname = \"c\"\nkind = \"independence\"\nmodules = [\"x\", \"y\"]\n");
    assert_eq!(d.message, "duplicate contract name `c`");
    // Points at the second `[[contracts]]` header.
    assert_eq!(d.line, 6);
}

// ----- rules -----

#[test]
fn selectors() {
    let p = |s: &str| s.parse::<RuleSelector>().unwrap();
    assert_eq!(p("ALL"), RuleSelector::All);
    assert_eq!(p("PM"), RuleSelector::Prefix { value: 0, digits: 0 });
    assert_eq!(p("PM01"), RuleSelector::Prefix { value: 1, digits: 2 });
    for s in ["ALL", "PM", "PM1", "PM01", "PM101", "PM007"] {
        assert_eq!(p(s).to_string(), s);
    }
    for s in ["", "all", "PM1a", "PM1000", "XX1", "PM-1", " PM1"] {
        assert!(s.parse::<RuleSelector>().is_err(), "{s:?}");
    }
    assert_eq!("all".parse::<RuleSelector>().unwrap_err().suggestion.as_deref(), Some("ALL"));
    assert!(p("PM1").matches(101) && p("PM1").matches(199) && !p("PM1").matches(201));
    assert!(p("PM01").matches(10) && p("PM01").matches(19) && !p("PM01").matches(101));
    assert!(p("PM101").matches(101) && !p("PM101").matches(102));
    assert!(p("PM").matches(0) && p("ALL").matches(999));
    assert!(p("PM0").matches(5) && !p("PM0").matches(100));
}

#[test]
fn rule_enablement() {
    let sel = |v: &[&str]| v.iter().map(|s| s.parse().unwrap()).collect::<Vec<RuleSelector>>();
    let mut r = RuleConfig::default();
    assert!(r.is_enabled(101, true));
    assert!(!r.is_enabled(101, false));
    r.extend_select = sel(&["PM5"]);
    assert!(r.is_enabled(501, false));
    r.ignore = sel(&["PM1"]);
    assert!(!r.is_enabled(101, true));
    // Ignoring at the default level wins over the implicit default selection.
    r.ignore = sel(&["ALL"]);
    assert!(!r.is_enabled(101, true));
    // More specific select beats a broader ignore.
    r.select = Some(sel(&["PM101"]));
    r.ignore = sel(&["PM1"]);
    assert!(r.is_enabled(101, false));
    assert!(!r.is_enabled(102, true));
    // Tie: ignore wins.
    r.select = Some(sel(&["PM1"]));
    assert!(!r.is_enabled(101, true));
    // Explicit select replaces the defaults.
    r.select = Some(sel(&["PM2"]));
    r.ignore.clear();
    r.extend_select.clear();
    assert!(!r.is_enabled(301, true));
    assert!(r.is_enabled(201, false));
    r.select = Some(Vec::new());
    assert!(!r.is_enabled(201, true));
}

// ----- entrypoints -----

#[test]
fn entrypoint_matcher_merges_presets() {
    let mut cfg = ok("presets = [\"flask\", \"pytest\"]\n[entrypoints]\ndecorators = [\"celery.task\"]\nbases = [\"my.Base\"]\nnames = [\"main\", \"pkg.mod:Class.*\"]\n");
    let m = cfg.entrypoint_matcher();
    assert!(m.decorator(b"app.route"));
    assert!(m.decorator(b"bp.before_request"));
    assert!(m.decorator(b"pytest.fixture"));
    assert!(m.decorator(b"celery.task"));
    assert!(!m.decorator(b"route"));
    assert!(!m.decorator(b"functools.cache"));
    assert!(m.base(b"flask.views.MethodView"));
    assert!(m.base(b"unittest.TestCase"));
    assert!(m.base(b"my.Base"));
    assert!(!m.base(b"django.db.models.Model"));
    assert!(m.name(b"test_foo", b"t.test_mod:test_foo"));
    assert!(m.name(b"main", b"pkg:main"));
    assert!(m.name(b"meth", b"pkg.mod:Class.meth"));
    assert!(!m.name(b"meth", b"pkg.mod:Other.meth"));
    assert!(!m.name(b"helper", b"pkg:helper"));
    assert!(!m.is_empty());

    cfg.presets = vec![Preset::Django];
    cfg.entrypoints = EntrypointConfig::default();
    let m = cfg.entrypoint_matcher();
    assert!(m.base(b"django.views.generic.ListView"));
    assert!(m.name(b"clean_email", b"app.forms:F.clean_email"));
    assert!(!m.decorator(b"app.route"));

    cfg.presets.clear();
    assert!(cfg.entrypoint_matcher().is_empty());
}

/// The matcher records which user patterns matched (presets excluded); a dotted name without
/// `:` covers the module and everything in it.
#[test]
fn entrypoint_matcher_tracks_unmatched_patterns() {
    let cfg = ok("presets = [\"pytest\"]\n[entrypoints]\ndecorators = [\"celery.task\", \"never.*\"]\nbases = [\"my.Base\"]\nmembers = [\"on_*\"]\nnames = [\"main\", \"pkg.cli\", \"*.views\", \"pkg.mod:Class.*\", \"nope\"]\n");
    let m = cfg.entrypoint_matcher();
    let unmatched = |m: &EntrypointMatcher| m.unmatched().map(|(k, g)| format!("{k}={g}")).collect::<Vec<_>>();
    assert_eq!(unmatched(&m).len(), 9);
    // Preset hits do not count for user patterns.
    assert!(m.decorator(b"pytest.fixture"));
    assert!(m.name(b"test_x", b"t:test_x"));
    assert_eq!(unmatched(&m).len(), 9);
    assert!(m.decorator(b"celery.task") && m.base(b"my.Base") && m.member(b"on_save"));
    // Module patterns: the module itself and its members.
    assert!(m.qualified_name(b"pkg.cli:main.inner"));
    assert!(m.qualified_name(b"app.views:index"));
    assert!(m.qualified_name(b"app.views"));
    assert!(!m.qualified_name(b"pkg.client:x"));
    // Both sides of `name` are evaluated: `main` (simple) and `pkg.mod:Class.*` both match.
    assert!(m.name(b"main", b"pkg.mod:Class.main"));
    assert_eq!(unmatched(&m), ["decorators=never.*", "names=nope"]);
    let copy = m.clone();
    assert!(m.decorator(b"never.x") && m.simple_name(b"nope"));
    assert!(unmatched(&m).is_empty());
    assert_eq!(unmatched(&copy).len(), 2);
}

/// Entrypoint patterns are `Glob`s, so code that edits a `Config` directly cannot smuggle in
/// an invalid pattern, and building the matcher never panics.
#[test]
fn entrypoint_globs_are_validated_by_type() {
    assert_eq!("a b".parse::<Glob>(), Err(GlobError::BadChar(' ')));
    assert_eq!(Glob::new("x[y]"), Err(GlobError::BadChar('[')));
    assert_eq!(Glob::new(""), Err(GlobError::Empty));

    let mut cfg = Config::new(&base());
    cfg.entrypoints.names.push(Glob::new("handle_*").unwrap());
    let m = cfg.entrypoint_matcher();
    assert!(!m.has_qualified_names());
    assert!(m.simple_name(b"handle_x") && !m.simple_name(b"other"));

    cfg.presets = Preset::ALL.to_vec();
    cfg.entrypoints.names.push("pkg.mod:run_*".parse().unwrap());
    cfg.entrypoints.decorators.push("*.task".parse().unwrap());
    cfg.entrypoints.bases.push("my.Base".parse().unwrap());
    let m = cfg.entrypoint_matcher();
    assert!(m.has_qualified_names());
    assert!(m.qualified_name(b"pkg.mod:run_x") && !m.qualified_name(b"pkg.mod:walk"));
    assert!(m.name(b"handle_x", b"a:handle_x") && m.decorator(b"celery.task") && m.base(b"my.Base"));

    // Edited configs serialise and re-parse to the same value.
    let text = cfg.to_toml();
    assert_eq!(ok(&text), cfg, "{text}");
}

#[test]
fn every_preset_compiles_into_a_matcher() {
    let mut cfg = Config::new(&base());
    cfg.presets = Preset::ALL.to_vec();
    let m = cfg.entrypoint_matcher();
    assert!(m.decorator(b"router.get") && m.decorator(b"click.group"));
    // Pydantic: validators and hooks are members (live with their model); models are not roots.
    assert!(m.member(b"field_validator") && m.member(b"pydantic.model_validator") && m.member(b"model_post_init"));
    assert!(!m.decorator(b"field_validator") && !m.base(b"pydantic.BaseModel"));
}

// ----- misc -----

#[test]
fn normalize_paths() {
    let n = |s: &str| normalize(Path::new(s));
    assert_eq!(n("/a/./b/../c"), PathBuf::from("/a/c"));
    assert_eq!(n("/.."), PathBuf::from("/"));
    assert_eq!(n("a/../.."), PathBuf::from(".."));
    assert_eq!(n("../x"), PathBuf::from("../x"));
    assert_eq!(n("."), PathBuf::from("."));
    assert_eq!(n("a/b/"), PathBuf::from("a/b"));
}

#[test]
fn python_versions() {
    assert_eq!("3.13".parse(), Ok(PythonVersion { major: 3, minor: 13 }));
    assert_eq!("3.0".parse(), Ok(PythonVersion { major: 3, minor: 0 }));
    for bad in ["3", "3.", ".1", "3.x", "3.13.1", "3.1000", " 3.1", "+3.1"] {
        assert!(matches!(bad.parse::<PythonVersion>(), Err(PythonVersionError::Syntax(_))), "{bad}");
    }
    assert!(matches!("2.7".parse::<PythonVersion>(), Err(PythonVersionError::Unsupported(_))));
}

// ----- discovery and overrides (real filesystem) -----

/// A unique temporary directory, removed on drop.
struct TempDir(PathBuf);

impl TempDir {
    fn new() -> TempDir {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.subsec_nanos());
        let dir = std::env::temp_dir().join(format!("pymapper-config-{}-{n}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // Canonical so it compares equal to discovered paths (macOS /var -> /private/var).
        TempDir(dir.canonicalize().unwrap())
    }

    fn write(&self, rel: &str, text: &str) -> PathBuf {
        let p = self.0.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, text).unwrap();
        p
    }

    fn mkdir(&self, rel: &str) -> PathBuf {
        let p = self.0.join(rel);
        std::fs::create_dir_all(&p).unwrap();
        p
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn load_in(cwd: &Path, o: &Overrides) -> Loaded {
    load(cwd, o).unwrap_or_else(|e| panic!("{e}"))
}

#[test]
fn discovers_pyproject_upwards() {
    let t = TempDir::new();
    t.write("pyproject.toml", "[project]\nname = \"x\"\n\n[tool.pymapper]\nthreads = 3\nsrc = [\"src\"]\n\n[tool.pymapper.rules]\nignore = [\"PM1\"]\n");
    let deep = t.mkdir("src/pkg/sub");
    let l = load_in(&deep, &Overrides::default());
    assert_eq!(l.project.root, t.0);
    assert_eq!(l.project.config_file, Some((t.0.join("pyproject.toml"), ConfigFileKind::Pyproject)));
    assert_eq!(l.project.pyproject, Some(t.0.join("pyproject.toml")));
    assert_eq!(l.config.threads, 3);
    // Relative paths resolve against the config file's directory, not the cwd.
    assert_eq!(l.config.src, vec![t.0.join("src")]);
    assert_eq!(l.config.paths, vec![t.0.clone()]);
    assert_eq!(l.config.cache_dir, t.0.join(".pymapper_cache"));
    assert_eq!(l.config.rules.ignore.len(), 1);
    assert!(l.warnings.is_empty());
}

#[test]
fn pyproject_without_section_is_skipped() {
    let t = TempDir::new();
    t.write("pymapper.toml", "threads = 5\n");
    t.write("sub/pyproject.toml", "[project]\nname = \"sub\"\n[tool.ruff]\nline-length = 1\n");
    // Mentions pymapper, but only in another table: parsed, no section.
    t.write("sub/inner/pyproject.toml", "[project]\nname = \"pymapper-plugin\"\n");
    let l = load_in(&t.0.join("sub/inner"), &Overrides::default());
    assert_eq!(l.config.threads, 5);
    assert_eq!(l.project.root, t.0);
    assert_eq!(l.project.config_file.unwrap().1, ConfigFileKind::Standalone);
    assert_eq!(l.project.pyproject, None);
}

#[test]
fn standalone_wins_over_pyproject_with_warning() {
    let t = TempDir::new();
    t.write("pymapper.toml", "threads = 1\n");
    t.write("pyproject.toml", "[tool.pymapper]\nthreads = 2\n");
    let l = load_in(&t.0, &Overrides::default());
    assert_eq!(l.config.threads, 1);
    assert_eq!(l.warnings.len(), 1);
    assert!(l.warnings[0].contains("takes precedence"), "{}", l.warnings[0]);
    assert_eq!(l.project.pyproject, Some(t.0.join("pyproject.toml")));
}

#[test]
fn no_config_uses_nearest_pyproject_as_root() {
    let t = TempDir::new();
    t.write("pyproject.toml", "[project]\nname = \"x\"\n");
    let pkg = t.mkdir("pkg");
    let l = load_in(&pkg, &Overrides::default());
    assert_eq!(l.project.root, t.0);
    assert_eq!(l.project.config_file, None);
    assert_eq!(l.config, Config::new(&t.0));
}

#[test]
fn start_from_first_path_and_files() {
    let t = TempDir::new();
    t.write("proj/pymapper.toml", "threads = 7\n");
    let file = t.write("proj/pkg/mod.py", "x = 1\n");
    let o = Overrides { paths: vec![PathBuf::from("proj/pkg/mod.py"), PathBuf::from("other")], ..Overrides::default() };
    let l = load_in(&t.0, &o);
    assert_eq!(l.config.threads, 7);
    assert_eq!(l.project.root, t.0.join("proj"));
    // CLI paths are relative to the cwd and become the report scope; the configured `paths`
    // (the project root) stay. A path outside the project root is not loaded.
    assert_eq!(l.scope, vec![file.clone(), t.0.join("other")]);
    assert_eq!(l.config.paths, vec![t.0.join("proj")]);
    // A path inside the root that the configured `paths` do not cover is loaded too.
    t.write("proj/pymapper.toml", "paths = [\"pkg\"]\n");
    let l = load_in(&t.0, &Overrides { paths: vec!["proj/tests".into(), "proj/pkg/mod.py".into()], ..Overrides::default() });
    assert_eq!(l.config.paths, vec![t.0.join("proj/pkg"), t.0.join("proj/tests")]);
    t.write("proj/pymapper.toml", "threads = 7\n");
    // Duplicates are dropped; covered paths are not added again.
    let o = Overrides { paths: vec!["proj/pkg".into(), "proj/pkg/./".into(), "proj/pkg/mod.py".into()], ..Overrides::default() };
    let l = load_in(&t.0, &o);
    assert_eq!(l.scope, vec![t.0.join("proj/pkg"), file]);
    assert_eq!(l.config.paths, vec![t.0.join("proj")]);
}

#[test]
fn project_root_does_not_depend_on_the_start_depth() {
    let t = TempDir::new();
    t.write("pyproject.toml", "[project]\nname = \"mono\"\n[tool.uv.workspace]\nmembers = [\"libs/*\"]\n");
    t.write("libs/a/pyproject.toml", "[project]\nname = \"a\"\n");
    let file = t.write("libs/a/ns/a/mod.py", "x = 1\n");
    for start in [PathBuf::from("."), PathBuf::from("libs/a"), PathBuf::from("libs/a/ns/a/mod.py")] {
        let l = load_in(&t.0, &Overrides { paths: vec![start.clone()], ..Overrides::default() });
        assert_eq!(l.project.root, t.0, "{start:?}");
        assert_eq!(l.config.paths, vec![t.0.clone()], "{start:?}");
        assert_eq!(l.config.cache_dir, t.0.join(".pymapper_cache"), "{start:?}");
    }
    let l = load_in(&t.0.join("libs/a/ns"), &Overrides::default());
    assert_eq!(l.project.root, t.0);
    assert!(l.scope.is_empty());
    // A discovered config file still pins the root to its directory.
    t.write("libs/a/pymapper.toml", "threads = 2\n");
    let l = load_in(&t.0, &Overrides { paths: vec![file], ..Overrides::default() });
    assert_eq!(l.project.root, t.0.join("libs/a"));
    assert_eq!(l.config.threads, 2);
    // ... unless config files are ignored.
    let l = load_in(&t.0.join("libs/a"), &Overrides { isolated: true, ..Overrides::default() });
    assert_eq!(l.project.root, t.0);
}

#[test]
fn isolated_and_explicit_config() {
    let t = TempDir::new();
    t.write("pymapper.toml", "threads = 7\n");
    t.write("conf/custom.toml", "threads = 9\ncache-dir = \"c\"\n");
    t.write("conf/pyproject.toml", "[tool.pymapper]\nthreads = 11\n");
    t.write("conf/bare/pyproject.toml", "[project]\nname = \"x\"\n");

    let l = load_in(&t.0, &Overrides { isolated: true, ..Overrides::default() });
    assert_eq!(l.config.threads, 0);
    assert_eq!(l.project.config_file, None);

    let l = load_in(&t.0, &Overrides { config_file: Some("conf/custom.toml".into()), ..Overrides::default() });
    assert_eq!(l.config.threads, 9);
    assert_eq!(l.project.root, t.0.join("conf"));
    assert_eq!(l.config.cache_dir, t.0.join("conf/c"));

    let l = load_in(&t.0, &Overrides { config_file: Some("conf/pyproject.toml".into()), ..Overrides::default() });
    assert_eq!(l.config.threads, 11);
    assert_eq!(l.project.config_file.unwrap().1, ConfigFileKind::Pyproject);

    let l = load_in(&t.0, &Overrides { config_file: Some("conf/bare/pyproject.toml".into()), ..Overrides::default() });
    assert_eq!(l.config.threads, 0);
    assert!(l.warnings[0].contains("no [tool.pymapper] table"), "{:?}", l.warnings);

    let err = load(&t.0, &Overrides { config_file: Some("missing.toml".into()), ..Overrides::default() }).unwrap_err();
    assert!(matches!(err, ConfigError::Io { .. }));
    assert!(err.to_string().starts_with("cannot read "), "{err}");
}

#[test]
fn invalid_pyproject_reports_file_positions() {
    let t = TempDir::new();
    t.write("pyproject.toml", "[project]\nname = \"x\"\n\n[tool.pymapper]\nthreds = 3\n");
    let err = load(&t.0, &Overrides::default()).unwrap_err();
    let ConfigError::Invalid { diagnostics } = &err else { panic!("{err}") };
    assert_eq!(diagnostics.len(), 1);
    assert_eq!(diagnostics[0].origin, t.0.join("pyproject.toml").display().to_string());
    assert_eq!((diagnostics[0].line, diagnostics[0].col), (5, 1));
    assert_eq!(diagnostics[0].help.as_deref(), Some("did you mean `threads`?"));

    t.write("pyproject.toml", "[tool]\npymapper = 3\n");
    let err = load(&t.0, &Overrides::default()).unwrap_err();
    assert!(err.to_string().contains("`tool.pymapper`: expected a table, found an integer"), "{err}");

    // Broken TOML that mentions us is reported, not skipped.
    t.write("pyproject.toml", "[tool.pymapper\nthreads = 3\n");
    let err = load(&t.0, &Overrides::default()).unwrap_err();
    assert!(err.to_string().contains("invalid TOML"), "{err}");
}

#[test]
fn out_of_range_typed_overrides_are_rejected() {
    let t = TempDir::new();
    let err = load(&t.0, &Overrides { min_confidence: Some(101), ..Overrides::default() }).unwrap_err();
    assert!(matches!(err, ConfigError::Override { field: "min_confidence", .. }), "{err}");
    assert_eq!(err.to_string(), "invalid override `min_confidence`: 101 is out of range (0..=100)");
    let err = load(&t.0, &Overrides { threads: Some(MAX_THREADS + 1), ..Overrides::default() }).unwrap_err();
    assert!(matches!(err, ConfigError::Override { field: "threads", .. }), "{err}");
    let l = load_in(&t.0, &Overrides { min_confidence: Some(100), threads: Some(MAX_THREADS), ..Overrides::default() });
    assert_eq!((l.config.min_confidence, l.config.threads), (100, MAX_THREADS));
}

#[test]
fn overrides_layer_over_file() {
    let t = TempDir::new();
    t.write("pymapper.toml", "threads = 2\nmin-confidence = 50\npresets = [\"flask\"]\n[rules]\nselect = [\"PM1\"]\nignore = [\"PM101\"]\n[thresholds]\nmax-mccabe = 20\nmax-nesting = 6\n");
    let o = Overrides {
        set: vec!["thresholds.max-mccabe = 12".into(), "rules.ignore = [\"PM102\"]\nmode = \"library\"".into()],
        extend_select: vec!["PM9".parse().unwrap()],
        ignore: vec!["PM103".parse().unwrap()],
        presets: vec![Preset::Flask, Preset::Click],
        min_confidence: Some(90),
        output_format: Some(OutputFormat::Github),
        cache: Some(false),
        cache_dir: Some("cachehere".into()),
        include_deps: Some(true),
        include_stdlib: Some(true),
        venv: Some("v".into()),
        mode: Some(Mode::Application),
        threads: Some(4),
        extend_exclude: vec!["gen".into()],
        ..Overrides::default()
    };
    let sub = t.mkdir("sub");
    let l = load_in(&sub, &o);
    let c = &l.config;
    // `--set` replaced one key of [thresholds]; the other file key survives.
    assert_eq!(c.thresholds.max_mccabe, 12);
    assert_eq!(c.thresholds.max_nesting, 6);
    // `--set` replaced the ignore array, then typed flags appended.
    assert_eq!(c.rules.ignore.iter().map(ToString::to_string).collect::<Vec<_>>(), ["PM102", "PM103"]);
    assert_eq!(c.rules.select, Some(vec!["PM1".parse().unwrap()]));
    assert_eq!(c.rules.extend_select.len(), 1);
    assert_eq!(c.presets, vec![Preset::Flask, Preset::Click]);
    assert_eq!(c.min_confidence, 90);
    assert_eq!(c.output_format, OutputFormat::Github);
    assert!(!c.cache && c.include_deps && c.include_stdlib);
    // Typed path overrides resolve against the cwd.
    assert_eq!(c.cache_dir, sub.join("cachehere"));
    assert_eq!(c.venv, Some(sub.join("v")));
    // Typed flags beat `--set`.
    assert_eq!(c.mode, Mode::Application);
    assert_eq!(c.threads, 4);
    assert_eq!(c.extend_exclude, ["gen"]);

    let o = Overrides { select: Some(vec![RuleSelector::All]), ..Overrides::default() };
    assert_eq!(load_in(&t.0, &o).config.rules.select, Some(vec![RuleSelector::All]));
}

#[test]
fn set_fragment_errors_name_their_source() {
    let t = TempDir::new();
    t.write("pymapper.toml", "threads = 2\nzzz = 1\n");
    let o = Overrides { set: vec!["threads = 1".into(), "thresholds.max-mccab = 3".into()], ..Overrides::default() };
    let err = load(&t.0, &o).unwrap_err();
    let ConfigError::Invalid { diagnostics } = &err else { panic!("{err}") };
    assert_eq!(diagnostics.len(), 2, "{diagnostics:#?}");
    assert!(diagnostics[0].origin.ends_with("pymapper.toml"));
    assert_eq!(diagnostics[1].origin, "--set #2 `thresholds.max-mccab = 3`");
    assert_eq!((diagnostics[1].line, diagnostics[1].col), (1, 12));
    assert_eq!(diagnostics[1].help.as_deref(), Some("did you mean `thresholds.max-mccabe`?"));
}

#[test]
fn output_defaults_and_errors() {
    let cfg = ok("");
    assert_eq!(cfg.output_format, OutputFormat::Human);
    assert_eq!((cfg.group_by, cfg.color, cfg.limit), (GroupBy::Category, ColorChoice::Auto, DEFAULT_LIMIT));
    assert_eq!(ok("output-format = \"llm\"").output_format, OutputFormat::Llm);
    let d = one_err("group-by = \"files\"");
    assert!(d.message.contains("unknown grouping `files`") && d.message.contains("category, file"), "{}", d.message);
    let d = one_err("color = \"yes\"");
    assert!(d.message.contains("unknown color choice `yes`"), "{}", d.message);
    assert!(!one_err("limit = -1").message.is_empty());
}

#[test]
fn color_choice_resolution() {
    assert!(ColorChoice::Always.resolve(false));
    assert!(!ColorChoice::Never.resolve(true));
    assert!(!ColorChoice::Auto.resolve(false));
    // `auto` on a terminal depends on NO_COLOR, which other tests do not touch.
    let expect = std::env::var_os("NO_COLOR").is_none_or(|v| v.is_empty());
    assert_eq!(ColorChoice::Auto.resolve(true), expect);
}
