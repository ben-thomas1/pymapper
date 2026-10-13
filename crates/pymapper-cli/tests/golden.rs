//! Golden output of `pymapper check` on the demo project (`tests/corpus/demo`: one deliberate
//! problem per rule family), compared byte for byte with reviewed files in `tests/golden/demo/`.
//!
//! The concise, json, sarif and github goldens were produced by pymapper 0.3.0: those formats
//! must not change, except for the agreed 0.4.0 additions (json `root`, `id` and `fix`; sarif
//! `region.endLine`; github `endLine=`). The absolute demo root is written as `<root>`. `"<version>"` stands for
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
    // The absolute project root (JSON `root`, the llm header) depends on the checkout.
    let got = got.replace(&demo().display().to_string(), "<root>");
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

/// Runs `pymapper <args>` in `cwd`; returns (exit code, stdout, stderr).
fn pymapper(cwd: &Path, args: &[&str]) -> (i32, String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_pymapper"))
        .args(args)
        .current_dir(cwd)
        .env_remove("VIRTUAL_ENV")
        .env_remove("NO_COLOR")
        .output()
        .expect("run pymapper");
    let text = |b: Vec<u8>| String::from_utf8(b).expect("utf-8");
    (out.status.code().expect("exited normally"), text(out.stdout), text(out.stderr))
}

const NEXT: &str = "Next: pymapper explain <id> for evidence and source · suppress with # pymapper: ignore[PMxxx] on the reported line";

/// Finding ids at the start of llm item lines (`PM401-3fa2 ...`).
fn llm_ids(out: &str) -> Vec<String> {
    let mut ids = Vec::new();
    for line in out.lines() {
        let Some(word) = line.split(' ').next() else { continue };
        let b = word.as_bytes();
        if b.len() >= 10 && word.starts_with("PM") && b[2..5].iter().all(u8::is_ascii_digit) && b[5] == b'-' {
            ids.push(word.to_owned());
        }
    }
    ids
}

#[test]
fn llm_golden() {
    let (code, out) = check(&demo(), &["--llm"]);
    assert_eq!(code, 1);
    golden("llm.txt", &out);
    let (code, out) = check(&demo(), &["--llm", "--limit", "3"]);
    assert_eq!(code, 1);
    golden("llm-limit3.txt", &out);
    let (_, out) = check(&demo(), &["--llm", "--limit", "0", "--set", "analysis.dupes = true", "--min-confidence", "0"]);
    golden("llm-all.txt", &out);
}

#[test]
fn llm_header_states_mode_exit_meaning_and_blind_spots() {
    let (_, out) = check(&demo(), &["--llm"]);
    let head: Vec<&str> = out.lines().take(5).collect();
    assert!(head[0].starts_with("# pymapper check · root /"), "{out}");
    assert!(head[0].contains("(reached as .)"), "{out}");
    assert_eq!(head[1], "Dead-code mode: application ([project.scripts] in pyproject.toml)");
    assert!(head[2].starts_with("26 findings in 15 files, "), "{out}");
    assert!(head[2].contains("10 below confidence 80 not shown"), "{out}");
    assert!(head[2].ends_with("exit 1 = findings, not an error"), "{out}");
    assert!(head[3].starts_with("Not analysed: notebooks, C extensions, names used only via getattr"), "{out}");
    assert!(head[4].starts_with("Rules not run: PM501: needs `analysis.dupes = true`"), "{out}");
    // Sections in priority order; no concise footer or footnote after the trailer.
    let at = |s: &str| out.find(s).unwrap_or_else(|| panic!("{s} missing:\n{out}"));
    assert!(at("## Architecture") < at("## Complexity") && at("## Complexity") < at("## Dead code"));
    assert!(at("## Dead code") < at("## Unused imports"));
    assert!(out.ends_with(&format!("\n{NEXT}\n")), "{out}");
    assert!(!out.contains("Found "), "{out}");
    // From a subdirectory: the root as reached, and paths relative to the working directory.
    let (_, sub) = check(&demo().join("services"), &["--llm"]);
    assert!(sub.lines().next().is_some_and(|l| l.contains("(reached as ..)")), "{sub}");
    assert!(sub.contains("\nPM101-312c pricing.py:6-36 calculate_price: "), "{sub}");
    assert!(sub.contains("grep -rnw --exclude-dir=.venv dead_endpoint ..\n"), "{sub}");
}

#[test]
fn llm_cap_and_trailer() {
    let (_, out) = check(&demo(), &["--llm", "--limit", "3"]);
    assert_eq!(llm_ids(&out).len(), 3, "{out}");
    let lines: Vec<&str> = out.lines().collect();
    let n = lines.len();
    assert_eq!(lines[n - 1], NEXT);
    assert_eq!(
        lines[n - 2],
        "20 more findings not displayed (in 14 files). Use --limit 0 to show all, or --select <code> to narrow."
    );
    assert!(lines[2].contains(" items, showing 3 "), "{out}");
    // --limit 0 shows every item, without the trailer.
    let (_, all) = check(&demo(), &["--llm", "--limit", "0"]);
    let (_, default) = check(&demo(), &["--llm"]);
    assert_eq!(all, default, "the demo fits the default cap of 50");
    assert!(!all.contains("more findings not displayed"), "{all}");
    assert_eq!(llm_ids(&all).len(), 19, "{all}");
    // --limit 1: the most important item only (the contract break: severity error).
    let (_, one) = check(&demo(), &["--llm", "--limit", "1"]);
    assert_eq!(llm_ids(&one), ["PM303-e6ff"], "{one}");
    assert!(one.contains("\n25 more findings not displayed (in 15 files)."), "{one}");
}

#[test]
fn llm_merges_chains_metrics_and_pairs() {
    let (_, out) = check(&demo(), &["--llm", "--set", "analysis.dupes = true", "--min-confidence", "0", "--limit", "0"]);
    // A dead chain is one item: the root, then what only it uses.
    assert!(out.contains(
        "middleware/pipeline.py:21 format_receipt (function): never used (conf 100)\n  also dead if it goes: utils/formatting.py: format_currency:5\n"
    ), "{out}");
    assert!(!out.contains("\nPM401-") || !out.lines().any(|l| l.contains(" format_currency (function)")), "{out}");
    assert!(out.contains("verify: grep -rnwE --exclude-dir=.venv 'format_receipt|format_currency' .\n"), "{out}");
    // Metrics of one function: one line with every value and code.
    assert!(out.contains("calculate_price: McCabe 14>10, cognitive 32>15, nesting 5>4, params 8>7 (PM101,PM102,PM103,PM105)\n"), "{out}");
    // The duplicate pair once (PM501 is reported in both directions).
    assert_eq!(out.matches("duplicated lines: ").count(), 1, "{out}");
    assert!(out.contains("## Duplication (1 pair)"), "{out}");
}

#[test]
fn llm_ids_are_stable_and_match_json() {
    let (_, a) = check(&demo(), &["--llm", "--min-confidence", "0", "--limit", "0"]);
    let (_, b) = check(&demo(), &["--llm", "--min-confidence", "0", "--limit", "0"]);
    assert_eq!(a, b);
    let (_, json) = check(&demo(), &["--output-format", "json", "--min-confidence", "0"]);
    let ids = llm_ids(&a);
    assert!(ids.len() > 10, "{a}");
    for id in &ids {
        assert!(json.contains(&format!("\"id\":\"{id}\"")), "{id} not in the JSON output:\n{json}");
    }
    // The same ids from a subdirectory (ids use root-relative keys; the order follows the
    // displayed paths).
    let (_, sub) = check(&demo().join("services"), &["--llm", "--min-confidence", "0", "--limit", "0"]);
    let (mut want, mut got) = (ids, llm_ids(&sub));
    want.sort();
    got.sort();
    assert_eq!(got, want);
}

#[test]
fn llm_conflicts_with_machine_formats() {
    for format in ["json", "sarif", "github", "concise"] {
        let (code, _, err) = pymapper(&demo(), &["check", "--no-cache", "--llm", "--output-format", format]);
        assert_eq!(code, 2, "{format}: {err}");
    }
}

#[test]
fn json_has_root_ids_and_fixes() {
    let (_, out) = check(&demo(), &["--output-format", "json"]);
    assert!(out.contains(&format!("\"root\":\"{}\"", demo().display())), "{out}");
    let findings = out.lines().filter(|l| l.starts_with("  {\"code\"")).count();
    assert_eq!(out.matches("\"id\":\"PM").count(), findings);
    assert_eq!(out.matches("\"fix\":\"").count(), findings);
    assert!(out.contains("\"fix\":\"extract branches of `services.pricing:calculate_price` into helpers, use a dispatch table or return early, to bring McCabe from 14 to 10 or less\""), "{out}");
}
