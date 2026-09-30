use assert_cmd::Command;
use assert_cmd::cargo::cargo_bin_cmd;
use predicates::prelude::*;

fn pymapper() -> Command {
    cargo_bin_cmd!("pymapper")
}

#[test]
fn smoke_map() {
    pymapper()
        .args(["map", "tests/fixtures/simple.py"])
        .assert()
        .success()
        .stdout(predicate::str::contains("PyMapper v2 map"));
}

#[test]
fn smoke_map_json() {
    let output = pymapper()
        .args(["map", "--format", "json", "tests/fixtures/simple.py"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let parsed: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("should be valid JSON");
    assert_eq!(parsed["schema_version"], 2);
}

#[test]
fn smoke_scan() {
    pymapper()
        .args(["scan", "tests/fixtures/simple.py"])
        .assert()
        .success()
        .stdout(predicate::str::contains("PyMapper v2 scan"));
}

#[test]
fn smoke_explain() {
    pymapper()
        .args(["explain", "greet", "tests/fixtures/simple.py"])
        .assert()
        .success()
        .stdout(predicate::str::contains("metrics:"));
}
