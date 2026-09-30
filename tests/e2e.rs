use assert_cmd::Command;
use assert_cmd::cargo::cargo_bin_cmd;
use predicates::prelude::*;

fn pymapper() -> Command {
    cargo_bin_cmd!("pymapper")
}

#[test]
fn map_dot_produces_graph() {
    pymapper()
        .args(["map", "--format", "dot", "tests/fixtures/simple.py"])
        .assert()
        .success()
        .stdout(predicate::str::contains("digraph PyMapperV2"));
}

#[test]
fn scan_json_is_bundle() {
    let output = pymapper()
        .args(["scan", "--format", "json", "tests/fixtures/complex.py"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let parsed: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("should be valid JSON");
    assert!(parsed["symbols"].as_array().unwrap().len() > 0);
    assert!(parsed["findings"].as_array().unwrap().len() > 0);
}

#[test]
fn explain_unknown_symbol_fails() {
    pymapper()
        .args(["explain", "does_not_exist", "tests/fixtures/simple.py"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("no symbol matched"));
}

#[test]
fn confidence_filter_accepts_unresolved() {
    pymapper()
        .args([
            "map",
            "--confidence",
            "unresolved",
            "tests/fixtures/simple.py",
        ])
        .assert()
        .success();
}

#[test]
fn pytest_profile_marks_tests_as_entrypoints() {
    let dir = tempfile::tempdir().unwrap();
    let py = dir.path().join("test_sample.py");
    std::fs::write(&py, "def test_thing():\n    assert True\n").unwrap();

    let output = pymapper()
        .args([
            "scan",
            "--profile",
            "pytest",
            "--format",
            "json",
            py.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(output.status.success());
    let parsed: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("should be valid JSON");
    let edges = parsed["edges"].as_array().unwrap();
    assert!(edges.iter().any(|e| e["kind"] == "pytest-entrypoint"));
}
