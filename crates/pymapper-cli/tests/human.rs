//! Golden output of the default `human` format (ISSUES O1) on the demo project
//! (`tests/corpus/demo`), compared byte for byte with reviewed files in `tests/golden/demo/`.
//! Regenerate after an intended change with
//! `PYMAPPER_BLESS=1 cargo test -p pymapper-cli --test human` and review the diff.

use std::path::{Path, PathBuf};
use std::process::Command;

fn demo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/corpus/demo").canonicalize().expect("demo")
}

/// Runs `pymapper check --no-cache <args>` in the demo with a clean environment for the
/// layout (`NO_COLOR`, `COLUMNS`) plus `env`; returns (exit code, stdout).
fn check(args: &[&str], env: &[(&str, &str)]) -> (i32, String) {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_pymapper"));
    cmd.arg("check").arg("--no-cache").args(args).current_dir(demo());
    cmd.env_remove("VIRTUAL_ENV").env_remove("NO_COLOR").env_remove("COLUMNS");
    for (k, v) in env {
        cmd.env(k, v);
    }
    let out = cmd.output().expect("run pymapper");
    (out.status.code().expect("exited normally"), String::from_utf8(out.stdout).expect("utf-8 stdout"))
}

/// Compares `got` with `tests/golden/demo/<name>` (or writes it with `PYMAPPER_BLESS=1`).
fn golden(name: &str, got: &str) {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden/demo").join(name);
    if std::env::var_os("PYMAPPER_BLESS").is_some() {
        std::fs::write(&path, got).expect("write golden");
        return;
    }
    let want = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e} (bless with PYMAPPER_BLESS=1)", path.display()));
    if got != want {
        let first = got.lines().zip(want.lines()).position(|(a, b)| a != b).unwrap_or(got.lines().count().min(want.lines().count()));
        panic!(
            "{name} differs from the golden file at line {}:\n got: {:?}\nwant: {:?}\n--- full output ---\n{got}",
            first + 1,
            got.lines().nth(first),
            want.lines().nth(first)
        );
    }
}

#[test]
fn category_layout_is_the_default() {
    let (code, out) = check(&[], &[]);
    assert_eq!(code, 1);
    // Piped stdout with the default `color = "auto"`: no escapes.
    assert!(!out.contains('\x1b'), "{out}");
    golden("human.txt", &out);
    let (_, explicit) = check(&["--output-format", "human", "--group-by", "category", "--color", "never"], &[]);
    assert_eq!(out, explicit);
}

#[test]
fn file_layout() {
    let (code, out) = check(&["--group-by", "file"], &[]);
    assert_eq!(code, 1);
    golden("human-file.txt", &out);
}

#[test]
fn every_finding_with_duplicates() {
    let args = ["--set", "analysis.dupes = true", "--min-confidence", "0"];
    let (_, out) = check(&args, &[]);
    golden("human-dupes-all.txt", &out);
    // The pair (two PM501 findings, one per copy) is one line; the cycle and the merged
    // metrics are one line each.
    assert_eq!(out.matches('↔').count(), 1, "{out}");
    assert_eq!(out.matches("PM501-").count(), 1, "{out}");
    assert_eq!(out.matches("import cycle").count(), 1, "{out}");
    assert_eq!(out.matches("McCabe").count(), 1, "{out}");
    let (_, out) = check(&[&args[..], &["--group-by", "file"]].concat(), &[]);
    golden("human-file-dupes-all.txt", &out);
    assert_eq!(out.matches('↔').count(), 1, "{out}");
}

#[test]
fn colour_always_wins_over_no_color() {
    let (_, out) = check(&["--color", "always"], &[("NO_COLOR", "1")]);
    assert!(out.starts_with("\x1b[1;31mArchitecture\x1b[0m  2\n"), "{out:?}");
    golden("human-color.txt", &out);
    let (_, out) = check(&["--color", "always", "--group-by", "file"], &[]);
    golden("human-file-color.txt", &out);
    // `auto` honours NO_COLOR (and stdout is a pipe here anyway).
    let (_, out) = check(&[], &[("NO_COLOR", "1")]);
    assert!(!out.contains('\x1b'));
}

#[test]
fn ids_align_to_columns() {
    let (_, out) = check(&["--select", "PM405"], &[("COLUMNS", "60")]);
    let line = out.lines().find(|l| l.contains("PM405-")).expect("an unused import");
    assert_eq!(line.chars().count(), 60, "{line:?}");
    // Unparseable or tiny values fall back to the default width (100).
    let (_, out) = check(&["--select", "PM405"], &[("COLUMNS", "10")]);
    let line = out.lines().find(|l| l.contains("PM405-")).expect("an unused import");
    assert_eq!(line.chars().count(), 100, "{line:?}");
}
