//! Black-box tests of `xtask fuzz` using the planted-bug self-test targets.
//!
//! These run the (unwinding) debug binary with `--use-current-exe` so no fuzz-profile build is
//! triggered, and write artifacts to a scratch directory.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

fn scratch(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("xtask-fuzz-{tag}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("mkdir scratch");
    dir
}

fn xtask(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_xtask")).args(args).output().expect("spawn xtask")
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// `fuzz --use-current-exe --no-corpus --out <dir>` followed by `args`.
fn fuzz(out: &Path, args: &[&str]) -> Output {
    let mut all = vec!["fuzz", "--use-current-exe", "--no-corpus", "--out", out.to_str().unwrap()];
    all.extend_from_slice(args);
    xtask(&all)
}

/// Final (minimised) artifacts of one kind: `<kind>-<hash>.py`, sorted.
fn artifacts(dir: &Path, kind: &str) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = match fs::read_dir(dir) {
        Ok(rd) => rd.map(|e| e.expect("entry").path()).collect(),
        Err(_) => Vec::new(),
    };
    v.retain(|p| {
        let name = p.file_name().unwrap().to_string_lossy();
        name.starts_with(&format!("{kind}-")) && name.ends_with(".py") && !name.ends_with(".raw.py")
    });
    v.sort();
    v
}

/// Value of `key: value` in an artifact description.
fn info(py: &Path, key: &str) -> String {
    let txt = fs::read_to_string(py.with_extension("txt")).expect("artifact .txt");
    let prefix = format!("{key}: ");
    txt.lines().find_map(|l| l.strip_prefix(&prefix)).unwrap_or_else(|| panic!("no {key} in {txt}")).to_owned()
}

#[test]
fn list_shows_the_default_target_first() {
    let o = xtask(&["fuzz", "--list"]);
    assert_eq!(o.status.code(), Some(0));
    let out = text(&o.stdout);
    let names: Vec<&str> = out.lines().map(|l| l.split_whitespace().next().unwrap()).collect();
    assert_eq!(names[0], "parse", "{out}");
    assert!(names.contains(&"demo"), "{out}");
    for t in ["selftest-panic", "selftest-hang", "selftest-abort"] {
        assert!(names.contains(&t), "{out}");
    }
}

#[test]
fn clean_target_runs_the_exact_iteration_count_and_exits_0() {
    let dir = scratch("clean");
    let o = fuzz(&dir, &["-n", "3000", "-j", "3", "--seed", "5"]);
    assert_eq!(o.status.code(), Some(0), "stderr: {}", text(&o.stderr));
    let out = text(&o.stdout);
    assert!(out.contains("fuzz parse: 3000 execs"), "{out}");
    assert!(out.contains("seed 5, iterations 0..3000"), "{out}");
    assert!(out.contains("crashing inputs: 0 (0 distinct), hangs: 0"), "{out}");
    assert!(!dir.join("parse").exists(), "no artifacts for a clean run");
}

#[test]
fn crashes_are_saved_minimised_reproducible_and_replayable() {
    let dir = scratch("panic");
    let o = fuzz(&dir, &["-t", "selftest-panic", "-n", "20000", "-j", "4", "--seed", "1"]);
    assert_eq!(o.status.code(), Some(1), "stderr: {}", text(&o.stderr));
    let art = dir.join("selftest-panic");
    let found = artifacts(&art, "crash");
    assert_eq!(found.len(), 2, "{found:?}\n{}", text(&o.stdout));
    let mut minimised: Vec<Vec<u8>> = found.iter().map(|p| fs::read(p).unwrap()).collect();
    minimised.sort();
    assert_eq!(minimised, [b"\0".to_vec(), b"{{{{".to_vec()]);
    for py in &found {
        assert!(info(py, "location").contains("targets.rs:"));
        assert!(info(py, "message").starts_with("selftest: "));
        // The minimised input still crashes, and the raw input is what --replay regenerates.
        let r = fuzz(&dir, &["-t", "selftest-panic", "--reproduce", py.to_str().unwrap()]);
        assert_eq!(r.status.code(), Some(1));
        assert!(text(&r.stdout).contains(": crash at "), "{}", text(&r.stdout));
        let iter = info(py, "iteration");
        let replay = fuzz(&dir, &["-t", "selftest-panic", "--seed", "1", "--replay", &iter]);
        assert_eq!(replay.status.code(), Some(0));
        let raw = py.with_file_name(format!("{}.raw.py", py.file_stem().unwrap().to_string_lossy()));
        assert_eq!(replay.stdout, fs::read(&raw).unwrap(), "replay of iteration {iter}");
        assert!(info(py, "regenerate").ends_with(&format!("--seed 1 --replay {iter} --no-corpus")));
    }
    // A harmless file reproduces as ok.
    let ok = dir.join("ok.py");
    fs::write(&ok, b"x = 1\n").unwrap();
    let r = fuzz(&dir, &["-t", "selftest-panic", "--reproduce", ok.to_str().unwrap()]);
    assert_eq!(r.status.code(), Some(0));
    assert!(text(&r.stdout).ends_with(": ok\n"));
}

#[test]
fn reproduce_reads_stdin() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_xtask"))
        .args(["fuzz", "-t", "selftest-panic", "--reproduce", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(b"a{{{{b").unwrap();
    let o = child.wait_with_output().unwrap();
    assert_eq!(o.status.code(), Some(1));
    assert!(text(&o.stdout).starts_with("-: crash at "), "{}", text(&o.stdout));
}

#[test]
fn hangs_are_detected_and_minimised() {
    let dir = scratch("hang");
    let args = ["-t", "selftest-hang", "-n", "5000", "-j", "2", "--seed", "2", "--budget-ms", "150", "--max-hangs", "1"];
    let o = fuzz(&dir, &args);
    assert_eq!(o.status.code(), Some(1), "stderr: {}", text(&o.stderr));
    let out = text(&o.stdout);
    assert!(out.contains("stopped early: --max-hangs 1 reached"), "{out}");
    let found = artifacts(&dir.join("selftest-hang"), "hang");
    assert!(!found.is_empty(), "{out}");
    // Hangs minimise in fresh processes, down to the planted trigger (a UTF-8 BOM).
    assert_eq!(fs::read(&found[0]).unwrap(), "\u{FEFF}".as_bytes());
    assert_eq!(info(&found[0], "kind"), "hang");
}

#[test]
fn process_deaths_are_recovered_by_the_supervisor() {
    let dir = scratch("abort");
    let o = fuzz(&dir, &["-t", "selftest-abort", "-n", "4000", "-j", "2", "--seed", "3", "--max-restarts", "1"]);
    assert_eq!(o.status.code(), Some(1), "stderr: {}", text(&o.stderr));
    let out = text(&o.stdout);
    assert!(out.contains("process deaths: 2, explained by an input: 2, distinct inputs: 1"), "{out}");
    let found = artifacts(&dir.join("selftest-abort"), "abort");
    assert_eq!(found.len(), 1, "{out}");
    assert_eq!(fs::read(&found[0]).unwrap(), b"`");
    assert!(info(&found[0], "message").contains("stack overflow"), "{}", info(&found[0], "message"));
    let err = text(&o.stderr);
    assert!(err.contains("restarting from iteration"), "{err}");
    assert!(err.contains("not restarting (--max-restarts 1 reached)"), "{err}");
}

#[test]
fn extra_seed_files_and_dirs_are_loaded() {
    let dir = scratch("seeds");
    let sub = dir.join("pkg");
    fs::create_dir_all(&sub).unwrap();
    fs::write(sub.join("a.py"), b"a = 1\n").unwrap();
    fs::write(sub.join("b.pyi"), b"def b() -> int: ...\n").unwrap();
    fs::write(sub.join("notes.txt"), b"not python").unwrap();
    let single = dir.join("single.py");
    fs::write(&single, b"`\n").unwrap();
    // A relative single-file seed resolves against the working directory.
    let o = Command::new(env!("CARGO_BIN_EXE_xtask"))
        .args(["fuzz", "--use-current-exe", "--no-corpus", "-n", "200", "-j", "1", "--seed", "4", "--out"])
        .arg(&dir)
        .args(["--", "single.py"])
        .arg(&sub)
        .current_dir(&dir)
        .output()
        .unwrap();
    assert_eq!(o.status.code(), Some(0), "stderr: {}", text(&o.stderr));
    // 8 built-in seeds + single.py + a.py + b.pyi.
    assert!(text(&o.stderr).contains("| 11 seed inputs ("), "{}", text(&o.stderr));
}

#[test]
fn usage_errors_exit_2() {
    let dir = scratch("usage");
    for args in [
        &["-t", "nope", "-n", "10"][..],
        &["-T", "0"],
        &["-T", "nan"],
        &["--replay", "3"],
        &["--budget-ms", "0"],
        &["-j", "0"],
    ] {
        let o = fuzz(&dir, args);
        assert_eq!(o.status.code(), Some(2), "{args:?}: {}", text(&o.stderr));
    }
    let o = fuzz(&dir, &["-t", "nope"]);
    assert!(text(&o.stderr).contains("expected one of parse,"), "{}", text(&o.stderr));
}
