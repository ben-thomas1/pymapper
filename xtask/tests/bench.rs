//! Black-box tests of `xtask bench` against the small tests/corpus.
//!
//! These run the debug binary with `--use-current-exe` so no release build is triggered.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::Value;

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).parent().expect("xtask has a parent dir").to_path_buf()
}

fn scratch(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("xtask-bench-{tag}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("mkdir scratch");
    dir
}

fn xtask(args: &[&str], env: &[(&str, &str)]) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_xtask"));
    cmd.args(args);
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd.output().expect("spawn xtask")
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// Python files under tests/corpus, counted independently of the walker under test.
fn corpus_py_files(dir: &Path) -> u64 {
    let mut n = 0;
    for entry in fs::read_dir(dir).expect("read_dir") {
        let entry = entry.expect("entry");
        let ty = entry.file_type().expect("file type");
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if ty.is_dir() && name != "__pycache__" {
            n += corpus_py_files(&entry.path());
        } else if ty.is_file() && (name.ends_with(".py") || name.ends_with(".pyi")) {
            n += 1;
        }
    }
    n
}

fn bench_args<'a>(out: &'a str, baseline: &'a str) -> Vec<&'a str> {
    vec!["bench", "--use-current-exe", "-c", "tests", "-n", "2", "--warmup", "0", "--out", out, "--baseline", baseline]
}

#[test]
fn bench_writes_table_and_results_then_compares() {
    let dir = scratch("compare");
    let out = dir.join("latest.json");
    let base = dir.join("baseline.json");
    let (out_s, base_s) = (out.to_str().unwrap(), base.to_str().unwrap());

    // First run: no baseline yet.
    let o = xtask(&bench_args(out_s, base_s), &[]);
    assert_eq!(o.status.code(), Some(0), "stderr: {}", text(&o.stderr));
    let stdout = text(&o.stdout);
    let header = stdout.lines().next().expect("table header");
    for col in ["corpus", "target", "files", "MB", "walk", "read+parse", "total", "MB/s", "peak RSS MB"] {
        assert!(header.contains(col), "missing column {col}: {stdout}");
    }
    let row: Vec<&str> = stdout.lines().nth(1).unwrap().split_whitespace().collect();
    assert_eq!(row[..2], ["tests", "parse-only"], "{stdout}");
    assert!(stdout.contains("no baseline at"), "{stdout}");
    assert!(!base.exists());

    let results: Value = serde_json::from_str(&fs::read_to_string(&out).unwrap()).expect("results JSON");
    assert_eq!(results["schema"], 1);
    let entry = &results["results"][0];
    assert_eq!(entry["corpus"], "tests");
    assert_eq!(entry["target"], "parse-only");
    assert_eq!(entry["runs"], 2);
    assert_eq!(entry["files"].as_u64().unwrap(), corpus_py_files(&repo_root().join("tests/corpus")));
    assert!(entry["bytes"].as_u64().unwrap() > 0);
    assert!(entry["peak_rss_bytes"].as_u64().unwrap() > 0);
    let phases: Vec<&str> = entry["phases"].as_array().unwrap().iter().map(|p| p["name"].as_str().unwrap()).collect();
    assert_eq!(phases, ["walk", "read+parse"]);

    // Save a baseline, then compare against an identical-ish run: generous thresholds -> clean.
    let mut args = bench_args(out_s, base_s);
    args.push("--save-baseline");
    let o = xtask(&args, &[]);
    assert_eq!(o.status.code(), Some(0), "stderr: {}", text(&o.stderr));
    assert!(base.exists());
    let mut args = bench_args(out_s, base_s);
    args.extend(["--threshold", "1000000", "--min-delta-ms", "100000", "--min-delta-mb", "100000"]);
    let o = xtask(&args, &[]);
    assert_eq!(o.status.code(), Some(0), "stdout: {}", text(&o.stdout));
    let stdout = text(&o.stdout);
    assert!(stdout.contains("compared with"), "{stdout}");
    assert!(stdout.contains("0 regression(s)"), "{stdout}");

    // Doctor the baseline to be impossibly fast and small: every metric regresses, exit 1.
    let mut doctored: Value = serde_json::from_str(&fs::read_to_string(&base).unwrap()).unwrap();
    let e = &mut doctored["results"][0];
    e["total_s"] = 1e-9.into();
    e["peak_rss_bytes"] = 1.into();
    for p in e["phases"].as_array_mut().unwrap() {
        p["s"] = 1e-9.into();
    }
    fs::write(&base, serde_json::to_string(&doctored).unwrap()).unwrap();
    let mut args = bench_args(out_s, base_s);
    args.extend(["--min-delta-ms", "0", "--min-delta-mb", "0"]);
    let o = xtask(&args, &[]);
    assert_eq!(o.status.code(), Some(1), "stdout: {}", text(&o.stdout));
    let stdout = text(&o.stdout);
    assert_eq!(stdout.matches("REGRESSION").count(), 4, "{stdout}");
    assert!(stdout.contains("4 regression(s)"), "{stdout}");

    // --no-compare ignores the baseline.
    let mut args = bench_args(out_s, base_s);
    args.push("--no-compare");
    let o = xtask(&args, &[]);
    assert_eq!(o.status.code(), Some(0), "stdout: {}", text(&o.stdout));
    assert!(!text(&o.stdout).contains("compared with"));

    // A corrupt baseline is a fatal error.
    fs::write(&base, "{not json").unwrap();
    let o = xtask(&bench_args(out_s, base_s), &[]);
    assert_eq!(o.status.code(), Some(2));
    assert!(text(&o.stderr).contains("baseline"), "{}", text(&o.stderr));

    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn both_targets_and_explicit_threads() {
    let dir = scratch("targets");
    let out = dir.join("r.json");
    let o = xtask(
        &[
            "bench", "--use-current-exe", "-c", "tests", "-t", "read", "-t", "parse-only", "-t", "read", "-n", "1",
            "--warmup", "0", "-j", "2", "--no-compare", "--out", out.to_str().unwrap(),
        ],
        &[],
    );
    assert_eq!(o.status.code(), Some(0), "stderr: {}", text(&o.stderr));
    let results: Value = serde_json::from_str(&fs::read_to_string(&out).unwrap()).unwrap();
    let rows = results["results"].as_array().unwrap();
    let targets: Vec<&str> = rows.iter().map(|r| r["target"].as_str().unwrap()).collect();
    assert_eq!(targets, ["read", "parse-only"], "duplicate targets collapse");
    for r in rows {
        assert_eq!(r["threads"], 2);
        assert_eq!(r["files"], rows[0]["files"]);
        assert_eq!(r["bytes"], rows[0]["bytes"]);
    }
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn missing_corpora_are_skipped_with_a_note() {
    let dir = scratch("missing");
    let out = dir.join("r.json");
    let o = xtask(
        &["bench", "--use-current-exe", "-c", "stdlib", "-c", "ghost=/no/such/dir", "--out", out.to_str().unwrap()],
        &[("PYMAPPER_STDLIB", "/no/such/stdlib")],
    );
    assert_eq!(o.status.code(), Some(0), "stderr: {}", text(&o.stderr));
    let stderr = text(&o.stderr);
    assert!(stderr.contains("skipping corpus stdlib: /no/such/stdlib not found (set PYMAPPER_STDLIB)"), "{stderr}");
    assert!(stderr.contains("skipping corpus ghost: /no/such/dir not found"), "{stderr}");
    assert!(stderr.contains("nothing measured"), "{stderr}");
    assert!(!out.exists(), "nothing to write");
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn usage_errors_exit_2() {
    for args in [
        &["bench", "-c", "nonsense"][..],
        &["bench", "-t", "nonsense"],
        &["bench", "-n", "0"],
        &["bench", "-c", "tests", "-c", "tests"],
        &["bench", "--use-current-exe", "--threshold", "-1", "-c", "tests"],
    ] {
        let o = xtask(args, &[]);
        assert_eq!(o.status.code(), Some(2), "{args:?}: {}", text(&o.stderr));
    }
}

#[test]
fn bench_run_child_prints_one_json_record() {
    let root = repo_root().join("tests/corpus");
    let o = xtask(&["bench-run", "--target", "read", "--root", root.to_str().unwrap()], &[]);
    assert_eq!(o.status.code(), Some(0), "stderr: {}", text(&o.stderr));
    let stdout = text(&o.stdout);
    assert_eq!(stdout.lines().count(), 1, "{stdout}");
    let rec: Value = serde_json::from_str(&stdout).expect("record JSON");
    assert_eq!(rec["files"].as_u64().unwrap(), corpus_py_files(&root));
    assert_eq!(rec["errors"], 0);
    let phases: Vec<&str> = rec["phases"].as_array().unwrap().iter().map(|p| p["name"].as_str().unwrap()).collect();
    assert_eq!(phases, ["walk", "read"]);

    let o = xtask(&["bench-run", "--target", "read", "--root", "/no/such/root"], &[]);
    assert_eq!(o.status.code(), Some(2));
}
