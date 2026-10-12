//! Golden output of `pymapper check` on the demo project (`tests/corpus/demo`: one deliberate
//! problem per rule family), compared byte for byte with reviewed files in `tests/golden/demo/`.
//!
//! The concise, json, sarif and github goldens were produced by pymapper 0.3.0: those formats
//! must not change (only additive json fields, with a schema note). `"<version>"` stands for
//! the crate version. After an intended change, regenerate with
//! `PYMAPPER_BLESS=1 cargo test -p pymapper-cli --test golden` and review the diff.
//!
//! Also pins where paths are shown from (ISSUES O6): relative to the working directory in the
//! text formats, relative to the project root in json.

use std::path::{Path, PathBuf};
use std::process::Command;

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().expect("repo root")
}

fn demo() -> PathBuf {
    repo().join("tests/corpus/demo")
}

/// Runs `pymapper check --no-cache <args>` in `cwd`; returns (exit code, stdout).
fn check(cwd: &Path, args: &[&str]) -> (i32, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_pymapper"))
        .arg("check")
        .arg("--no-cache")
        .args(args)
        .current_dir(cwd)
        .env_remove("VIRTUAL_ENV")
        .env_remove("NO_COLOR")
        .output()
        .expect("run pymapper");
    let stdout = String::from_utf8(out.stdout).expect("utf-8 stdout");
    (out.status.code().expect("exited normally"), stdout)
}

/// Compares `got` with `tests/golden/demo/<name>` (or writes it with `PYMAPPER_BLESS=1`).
fn golden(name: &str, got: &str) {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden/demo").join(name);
    let got = got.replace(&format!("\"{}\"", env!("CARGO_PKG_VERSION")), "\"<version>\"");
    if std::env::var_os("PYMAPPER_BLESS").is_some() {
        std::fs::write(&path, &got).expect("write golden");
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
fn concise_is_unchanged_from_0_3_0() {
    let (code, out) = check(&demo(), &["--output-format", "concise"]);
    assert_eq!(code, 1);
    golden("concise.txt", &out);
    let (_, out) = check(&demo(), &["--output-format", "concise", "--set", "analysis.dupes = true", "--min-confidence", "0"]);
    golden("concise-dupes-all.txt", &out);
}

#[test]
fn machine_formats_are_unchanged_from_0_3_0() {
    for (format, name) in [("json", "json.json"), ("sarif", "sarif.json"), ("github", "github.txt")] {
        let (code, out) = check(&demo(), &["--output-format", format]);
        assert_eq!(code, 1, "{format}");
        golden(name, &out);
    }
}

#[test]
fn text_paths_are_relative_to_the_working_directory() {
    // From the repository root: paths carry the demo's directory.
    let (_, out) = check(&repo(), &["tests/corpus/demo", "--output-format", "concise"]);
    assert!(out.contains("\ntests/corpus/demo/services/pricing.py:1:8: PM405 `json` is imported but never used\n"), "{out}");
    // From a subdirectory: `../` for files outside it.
    let (_, out) = check(&demo().join("services"), &["--output-format", "concise"]);
    assert!(out.starts_with("../app.py:35:5: PM401"), "{out}");
    assert!(out.contains("\npricing.py:1:8: PM405"), "{out}");
    // So does the filter note.
    let (_, out) = check(&demo(), &["services", "--output-format", "concise"]);
    assert!(out.contains("Filtered to services ("), "{out}");
    // Messages naming a file follow the same base.
    let (_, out) = check(&demo().join("services"), &["--output-format", "concise", "--set", "analysis.dupes = true"]);
    assert!(out.contains("invoice.py:1:1: PM501 12 lines duplicated in pricing.py:39"), "{out}");
}

#[test]
fn json_paths_stay_relative_to_the_project_root() {
    let (_, from_root) = check(&demo(), &["--output-format", "json"]);
    let (_, from_sub) = check(&demo().join("services"), &["--output-format", "json"]);
    assert_eq!(from_root, from_sub);
    assert!(from_root.contains("\"path\":\"services/pricing.py\""), "{from_root}");
}

#[test]
fn baselines_do_not_depend_on_the_working_directory() {
    let dir = std::env::temp_dir().join(format!("pymapper-golden-baseline-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    let a = dir.join("a.json");
    let b = dir.join("b.json");
    check(&demo(), &["--save-baseline", a.to_str().expect("utf-8")]);
    check(&demo().join("services"), &["--save-baseline", b.to_str().expect("utf-8")]);
    let (a, b) = (std::fs::read(&a).expect("baseline a"), std::fs::read(&b).expect("baseline b"));
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(a, b);
}
