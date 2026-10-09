//! End-to-end behaviour of `xtask diff-ruff`.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn corpus() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../tests/corpus")
}

fn xtask(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_xtask")).arg("diff-ruff").args(args).output().expect("spawn xtask")
}

#[test]
fn default_run_diffs_our_extractor_with_zero_mismatches() {
    let root = corpus();
    let out = xtask(&[root.to_str().unwrap()]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(out.status.code(), Some(0), "{stdout}\n{}", String::from_utf8_lossy(&out.stderr));
    assert!(stdout.contains("facts (ruff/ours): "), "{stdout}");
    assert!(stdout.contains("mismatches: 0\n"), "{stdout}");
}

#[test]
fn dump_ours_prints_our_facts() {
    let file = corpus().join("fixtures/inheritance.py");
    let out = xtask(&[file.to_str().unwrap(), "--dump", "--ours"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(out.status.code(), Some(0), "{stdout}");
    assert!(stdout.lines().any(|l| l.starts_with("def class ")), "{stdout}");
}

#[test]
fn oracle_only_run_reports_ruff_facts() {
    let root = corpus();
    let out = xtask(&[root.to_str().unwrap(), "--oracle-only"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(out.status.code(), Some(0), "{stdout}\n{}", String::from_utf8_lossy(&out.stderr));
    assert!(stdout.starts_with("diff-ruff: "), "{stdout}");
    assert!(stdout.contains("ruff facts: defs "), "{stdout}");
    assert!(stdout.contains("candidate: none"), "{stdout}");
}

#[test]
fn self_check_has_zero_mismatches() {
    let root = corpus();
    let out = xtask(&[root.to_str().unwrap(), "--self-check", "--examples", "3"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(out.status.code(), Some(0), "{stdout}");
    assert!(stdout.contains("facts (ruff/ruff): "), "{stdout}");
    assert!(stdout.contains("mismatches: 0\n"), "{stdout}");
}

#[test]
fn dump_prints_facts_for_a_single_file() {
    let file = corpus().join("fixtures/inheritance.py");
    let out = xtask(&[file.to_str().unwrap(), "--dump"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(out.status.code(), Some(0), "{stdout}");
    assert!(stdout.starts_with("== "), "{stdout}");
    assert!(stdout.lines().any(|l| l.starts_with("def class ")), "{stdout}");
}

#[test]
fn missing_dir_is_a_fatal_error() {
    let out = xtask(&["/definitely/not/here"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("cannot scan"));
}

#[test]
fn dir_argument_is_required() {
    let out = xtask(&[]);
    assert_eq!(out.status.code(), Some(2));
}
