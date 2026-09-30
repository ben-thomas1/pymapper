use std::sync::Mutex;

use py_mapper::config::load_config;

// load_config uses std::env::current_dir, which is process-global.
// Serialize all config tests to avoid races with set_current_dir.
static DIR_LOCK: Mutex<()> = Mutex::new(());

#[test]
fn load_config_defaults_when_no_file() {
    let _lock = DIR_LOCK.lock().unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let orig = std::env::current_dir().unwrap();
    std::env::set_current_dir(tmp.path()).unwrap();

    let cfg = load_config();

    std::env::set_current_dir(orig).unwrap();
    let cfg = cfg.unwrap();

    assert!(cfg.report.fail_above.is_none());
    assert!(cfg.report.min_cc.is_none());
    assert!(cfg.report.top.is_none());
    assert!(!cfg.dead_code.exclude_overrides);
    assert!(cfg.dead_code.exclude_decorated.is_empty());
    assert!(!cfg.graph.no_externals);
}

#[test]
fn load_config_from_pymapper_toml() {
    let _lock = DIR_LOCK.lock().unwrap();
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(
        tmp.path().join(".pymapper.toml"),
        r#"
[report]
fail_above = "cc=10,cognitive=15"
min_cc = 3
sort = "cognitive"

[dead_code]
exclude_decorated = ["app.route", "click.command"]
exclude_overrides = true
profile = ["web"]

[graph]
no_externals = true
splines = "polyline"
"#,
    )
    .unwrap();

    let orig = std::env::current_dir().unwrap();
    std::env::set_current_dir(tmp.path()).unwrap();
    let cfg = load_config();
    std::env::set_current_dir(orig).unwrap();
    let cfg = cfg.unwrap();

    assert_eq!(cfg.report.fail_above.as_deref(), Some("cc=10,cognitive=15"));
    assert_eq!(cfg.report.min_cc, Some(3));
    assert_eq!(cfg.report.sort.as_deref(), Some("cognitive"));
    assert!(cfg.dead_code.exclude_overrides);
    assert_eq!(
        cfg.dead_code.exclude_decorated,
        vec!["app.route", "click.command"]
    );
    assert_eq!(cfg.dead_code.profile, vec!["web"]);
    assert!(cfg.graph.no_externals);
    assert_eq!(cfg.graph.splines.as_deref(), Some("polyline"));
}

#[test]
fn load_config_from_pyproject_toml() {
    let _lock = DIR_LOCK.lock().unwrap();
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(
        tmp.path().join("pyproject.toml"),
        r#"
[tool.pymapper.report]
fail_above = "cc=5"
min_cc = 2

[tool.pymapper.graph]
no_externals = true
"#,
    )
    .unwrap();

    let orig = std::env::current_dir().unwrap();
    std::env::set_current_dir(tmp.path()).unwrap();
    let cfg = load_config();
    std::env::set_current_dir(orig).unwrap();
    let cfg = cfg.unwrap();

    assert_eq!(cfg.report.fail_above.as_deref(), Some("cc=5"));
    assert_eq!(cfg.report.min_cc, Some(2));
    assert!(cfg.graph.no_externals);
}

#[test]
fn load_config_pymapper_toml_takes_precedence() {
    let _lock = DIR_LOCK.lock().unwrap();
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(
        tmp.path().join(".pymapper.toml"),
        r#"
[report]
min_cc = 10
"#,
    )
    .unwrap();
    std::fs::write(
        tmp.path().join("pyproject.toml"),
        r#"
[tool.pymapper.report]
min_cc = 99
"#,
    )
    .unwrap();

    let orig = std::env::current_dir().unwrap();
    std::env::set_current_dir(tmp.path()).unwrap();
    let cfg = load_config();
    std::env::set_current_dir(orig).unwrap();
    let cfg = cfg.unwrap();

    assert_eq!(cfg.report.min_cc, Some(10));
}

#[test]
fn load_config_pyproject_no_pymapper_section() {
    let _lock = DIR_LOCK.lock().unwrap();
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(
        tmp.path().join("pyproject.toml"),
        r#"
[tool.black]
line-length = 88
"#,
    )
    .unwrap();

    let orig = std::env::current_dir().unwrap();
    std::env::set_current_dir(tmp.path()).unwrap();
    let cfg = load_config();
    std::env::set_current_dir(orig).unwrap();
    let cfg = cfg.unwrap();

    // Should fall through to defaults
    assert!(cfg.report.fail_above.is_none());
    assert!(cfg.report.min_cc.is_none());
}

#[test]
fn load_config_pyproject_tool_no_pymapper() {
    let _lock = DIR_LOCK.lock().unwrap();
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(
        tmp.path().join("pyproject.toml"),
        r#"
[tool]
[tool.other]
key = "value"
"#,
    )
    .unwrap();

    let orig = std::env::current_dir().unwrap();
    std::env::set_current_dir(tmp.path()).unwrap();
    let cfg = load_config();
    std::env::set_current_dir(orig).unwrap();
    let cfg = cfg.unwrap();

    assert!(cfg.report.fail_above.is_none());
}

#[test]
fn load_config_invalid_toml_is_reported() {
    let _lock = DIR_LOCK.lock().unwrap();
    let tmp = tempfile::tempdir().unwrap();
    // Valid TOML with an invalid field type must not silently disable options.
    std::fs::write(
        tmp.path().join(".pymapper.toml"),
        r#"
[report]
min_cc = "not_a_number"
"#,
    )
    .unwrap();

    let orig = std::env::current_dir().unwrap();
    std::env::set_current_dir(tmp.path()).unwrap();
    let cfg = load_config();
    std::env::set_current_dir(orig).unwrap();
    assert!(cfg.is_err());
}
