//! End-to-end tests of `pymapper config` and the shared configuration flags.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// A unique temporary directory, removed on drop.
struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> TempDir {
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.subsec_nanos());
        let dir = std::env::temp_dir().join(format!("pymapper-cli-{tag}-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        TempDir(dir.canonicalize().unwrap())
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn run(cwd: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_pymapper")).current_dir(cwd).args(args).output().unwrap()
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

#[test]
fn template_is_printed_verbatim() {
    let t = TempDir::new("template");
    let o = run(&t.0, &["config", "--template"]);
    assert!(o.status.success());
    assert_eq!(stdout(&o), pymapper_core::config::TEMPLATE);
}

#[test]
fn effective_config_layers_file_set_and_flags() {
    let t = TempDir::new("layers");
    std::fs::write(t.0.join("pyproject.toml"), "[tool.pymapper]\nthreads = 2\nmode = \"library\"\n[tool.pymapper.thresholds]\nmax-nesting = 6\n").unwrap();
    std::fs::create_dir(t.0.join("pkg")).unwrap();
    let o = run(
        &t.0.join("pkg"),
        &["config", "--set", "thresholds.max-mccabe = 12", "--select", "PM1,PM20", "--preset", "django", "--deps", "-j", "5"],
    );
    let out = stdout(&o);
    assert!(o.status.success(), "{}", stderr(&o));
    assert!(out.contains(&format!("# config file: {}", t.0.join("pyproject.toml").display())), "{out}");
    for line in [
        "threads = 5",
        "mode = \"library\"",
        "max-nesting = 6",
        "max-mccabe = 12",
        "select = [\"PM1\", \"PM20\"]",
        "presets = [\"django\"]",
        "include-deps = true",
    ] {
        assert!(out.lines().any(|l| l == line), "missing `{line}` in\n{out}");
    }
}

#[test]
fn invalid_config_exits_2_with_suggestion() {
    let t = TempDir::new("invalid");
    std::fs::write(t.0.join("pymapper.toml"), "[rules]\nselct = [\"PM1\"]\n").unwrap();
    let o = run(&t.0, &["config"]);
    assert_eq!(o.status.code(), Some(2));
    let err = stderr(&o);
    assert!(err.contains("pymapper.toml:2:1: unknown option `rules.selct`"), "{err}");
    assert!(err.contains("help: did you mean `rules.select`?"), "{err}");

    // `--isolated` skips the broken file.
    let o = run(&t.0, &["config", "--isolated"]);
    assert!(o.status.success(), "{}", stderr(&o));
}

#[test]
fn bad_flag_values_are_usage_errors() {
    let t = TempDir::new("flags");
    for args in [
        &["config", "--select", "pm1"][..],
        &["config", "--output-format", "xml"],
        &["config", "--min-confidence", "101"],
        &["config", "--preset", "rails"],
        &["config", "--template", "--isolated"],
    ] {
        let o = run(&t.0, args);
        assert_eq!(o.status.code(), Some(2), "{args:?}: {}", stderr(&o));
    }
}
