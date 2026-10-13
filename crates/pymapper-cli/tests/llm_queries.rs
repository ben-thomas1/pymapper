//! Golden `--llm` output of the query subcommands, `deps` and `config` on the demo project
//! (`tests/corpus/demo`), and their cwd-relative paths (ISSUES O4, O6). Regenerate after an
//! intended change with `PYMAPPER_BLESS=1 cargo test -p pymapper-cli --test llm_queries` and
//! review the diff.

use std::path::{Path, PathBuf};
use std::process::Command;

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().expect("repo root")
}

fn demo() -> PathBuf {
    repo().join("tests/corpus/demo")
}

/// Runs `pymapper <args> --no-cache` in `cwd`; returns (exit code, stdout, stderr).
pub fn run(cwd: &Path, args: &[&str]) -> (i32, String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_pymapper"))
        .args(args)
        .arg("--no-cache")
        .current_dir(cwd)
        .env_remove("VIRTUAL_ENV")
        .env_remove("NO_COLOR")
        .output()
        .expect("run pymapper");
    let stdout = String::from_utf8(out.stdout).expect("utf-8 stdout");
    let stderr = String::from_utf8(out.stderr).expect("utf-8 stderr");
    (out.status.code().expect("exited normally"), stdout, stderr)
}

/// Compares `got` with `tests/golden/demo/<name>` (or writes it with `PYMAPPER_BLESS=1`).
pub fn golden(name: &str, got: &str) {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden/demo").join(name);
    let got = got.replace(&demo().display().to_string(), "<demo>");
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

/// Runs an llm command on the demo, checks its exit code and the shared conventions (header,
/// next step, no colour), and compares it with its golden file.
fn llm_golden(name: &str, args: &[&str], code: i32) -> String {
    let mut a = args.to_vec();
    a.push("--llm");
    let (c, out, err) = run(&demo(), &a);
    assert_eq!(c, code, "{args:?}\nstdout:\n{out}\nstderr:\n{err}");
    assert!(out.starts_with(&format!("# pymapper {}", args[0])), "{out}");
    assert!(out.contains("\nNext: "), "{out}");
    assert!(!out.contains('\u{1b}'), "escape codes in llm output:\n{out}");
    golden(name, &out);
    // Deterministic: a second run gives the same bytes.
    assert_eq!(run(&demo(), &a).1, out, "{args:?} is not deterministic");
    out
}

#[test]
fn callers_and_callees() {
    let out = llm_golden("callers-llm.txt", &["callers", "models.order:Order"], 0);
    // Grouped by caller module, possible calls counted, blind spots stated once.
    assert!(out.contains("5 callers in 3 files (4 possible)"), "{out}");
    assert!(out.contains("\n## services/pricing.py (services.pricing)\n"), "{out}");
    assert!(out.contains("Not call edges"), "{out}");
    llm_golden("callers-capped-llm.txt", &["callers", "models.order:Order", "--limit", "2"], 0);
    let out = llm_golden("callees-llm.txt", &["callees", "app:main"], 0);
    assert!(out.contains("# pymapper callees app:main\n"), "{out}");
}

#[test]
fn capped_lists_end_with_the_trailer() {
    let (_, out, _) = run(&demo(), &["callers", "models.order:Order", "--llm", "--limit", "2"]);
    assert!(out.contains("\n3 more callers not displayed. Use --limit 0 to show all.\n"), "{out}");
    let (_, all, _) = run(&demo(), &["callers", "models.order:Order", "--llm", "--limit", "0"]);
    assert!(!all.contains("not displayed"), "{all}");
}

#[test]
fn impact_shows_symbols_first_and_collapses_importers() {
    let out = llm_golden("impact-llm.txt", &["impact", "services.pricing:calculate_price"], 0);
    let symbols = out.find("Affected symbols").expect("symbols section");
    let importers = out.find("## Importing modules").expect("importers section");
    assert!(symbols < importers, "{out}");
    let out = llm_golden("impact-capped-llm.txt", &["impact", "models.base", "--limit", "3"], 0);
    assert!(out.contains("## Importing modules (9; first 3 shown, direct first)"), "{out}");
    assert!(out.contains("6 more importing modules not displayed"), "{out}");
    assert!(out.contains("5 more affected symbols not displayed. Use --limit 0 to show all."), "{out}");
}

#[test]
fn path_found_and_not_found() {
    llm_golden("path-llm.txt", &["path", "app:main", "services.pricing:calculate_price"], 0);
    let out = llm_golden("path-imports-llm.txt", &["path", "app", "models.base", "--imports"], 0);
    assert!(out.contains("[import, top-level]"), "{out}");
    let out = llm_golden("path-none-llm.txt", &["path", "services.pricing:legacy_discount", "app:main"], 1);
    assert!(out.contains("No call path (exit 1)"), "{out}");
}

#[test]
fn imports_and_cycles() {
    llm_golden("imports-llm.txt", &["imports", "services.pricing"], 0);
    let out = llm_golden("cycles-llm.txt", &["cycles"], 0);
    // Edge kinds and eager status.
    assert!(out.contains("2 modules, eager (every edge runs on import)"), "{out}");
    assert!(out.contains("repo.order_repo -> models.order (top-level) at repo/order_repo.py:1:26"), "{out}");
    llm_golden("cycles-calls-llm.txt", &["cycles", "--calls", "--symbol", "cycles:ping"], 0);
}

#[test]
fn summary_most_called_skips_possible_edges() {
    let out = llm_golden("summary-llm.txt", &["summary"], 0);
    // BaseModel.validate has 2 resolved callers and 3 possible ones: the text output counts
    // all 5, the llm ranking only the 2 (ISSUES F9).
    assert!(out.contains("BaseModel.validate: 2 callers (+3 possible)"), "{out}");
    let (_, text, _) = run(&demo(), &["summary"]);
    assert!(text.contains("most called models.base:BaseModel.validate (5 callers)"), "{text}");
}

#[test]
fn deps_and_config() {
    llm_golden("deps-llm.txt", &["deps"], 0);
    llm_golden("deps-score-llm.txt", &["deps", "--score"], 0);
    let out = llm_golden("config-llm.txt", &["config"], 0);
    assert!(out.contains("root: . · config file: pyproject.toml\n"), "{out}");
    assert!(out.contains("\n[thresholds]\n"), "{out}");
}

#[test]
fn query_paths_are_relative_to_the_working_directory() {
    let sub = demo().join("services");
    // Text and llm show `../` for files outside the working directory.
    let (_, text, _) = run(&sub, &["callers", "services.pricing:calculate_price"]);
    assert_eq!(text, "../app.py:31:11: app:main [certain]\n1 caller of services.pricing:calculate_price (function, pricing.py:6)\n");
    let (_, llm, _) = run(&sub, &["callers", "services.pricing:calculate_price", "--llm"]);
    assert!(llm.contains("(function, pricing.py:6) · root: .. ·"), "{llm}");
    assert!(llm.contains("\n## ../app.py (app)\n"), "{llm}");
    let (_, cycles, _) = run(&sub, &["cycles"]);
    assert!(cycles.starts_with("../repo/order_repo.py:1:26: import cycle"), "{cycles}");
    // JSON stays relative to the project root.
    let (_, a, _) = run(&sub, &["callers", "services.pricing:calculate_price", "--format", "json"]);
    let (_, b, _) = run(&demo(), &["callers", "services.pricing:calculate_price", "--format", "json"]);
    assert_eq!(a, b);
    assert!(a.contains("\"path\":\"app.py\""), "{a}");
}
