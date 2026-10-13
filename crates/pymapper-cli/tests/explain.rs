//! End-to-end tests of `pymapper explain` on the demo project (`tests/corpus/demo`: one
//! deliberate problem per rule family): every family's sections, ids computed through the
//! core API, symbol input, ambiguous and unknown targets (ISSUES O3). Goldens in
//! `tests/golden/demo/explain-*.txt`; regenerate with
//! `PYMAPPER_BLESS=1 cargo test -p pymapper-cli --test explain`.

use std::path::{Path, PathBuf};
use std::process::Command;

use pymapper_core::output::ids::stable_hash;

fn demo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/corpus/demo").canonicalize().expect("demo")
}

/// Runs `pymapper explain <args> --no-cache` in `cwd`; returns (exit code, stdout, stderr).
fn explain(cwd: &Path, args: &[&str]) -> (i32, String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_pymapper"))
        .arg("explain")
        .args(args)
        .arg("--no-cache")
        .current_dir(cwd)
        .env_remove("VIRTUAL_ENV")
        .output()
        .expect("run pymapper");
    let stdout = String::from_utf8(out.stdout).expect("utf-8 stdout");
    let stderr = String::from_utf8(out.stderr).expect("utf-8 stderr");
    (out.status.code().expect("exited normally"), stdout, stderr)
}

/// `explain` that must succeed; returns stdout.
#[track_caller]
fn ok(args: &[&str]) -> String {
    let (code, out, err) = explain(&demo(), args);
    assert_eq!(code, 0, "{args:?}\nstdout:\n{out}\nstderr:\n{err}");
    out
}

/// Compares `got` with `tests/golden/demo/<name>` (or writes it with `PYMAPPER_BLESS=1`).
fn golden(name: &str, got: &str) {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden/demo").join(name);
    if std::env::var_os("PYMAPPER_BLESS").is_some() {
        std::fs::write(&path, got).expect("write golden");
        return;
    }
    let want = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e} (bless with PYMAPPER_BLESS=1)", path.display()));
    assert!(got == want, "{name} differs from its golden file:\n{got}");
}

/// The id `check` gives a finding whose baseline key is `code` + `parts` (rule code, the
/// symbol's qualified id, the stable message arguments), through the core's frozen hash:
/// `PMxxx-` and the top 4 hex digits (any unique prefix of 4 or more resolves).
fn id_of(code: u16, parts: &[&str]) -> String {
    let mut key = code.to_le_bytes().to_vec();
    for p in parts {
        key.extend_from_slice(&(p.len() as u32).to_le_bytes());
        key.extend_from_slice(p.as_bytes());
    }
    format!("PM{code}-{}", &format!("{:016x}", stable_hash(&key))[..4])
}

#[test]
fn id_round_trip_through_the_core_hash() {
    let id = id_of(401, &["services.pricing:legacy_discount"]);
    let out = ok(&[&id]);
    assert!(out.starts_with(&format!("{id} · Dead code · unused-function: function services.pricing:legacy_discount is never used\n")), "{out}");
    // The symbol's own explain shows the same id; lower case and a longer prefix work too.
    assert!(ok(&["services.pricing:legacy_discount"]).contains(&format!("\n{id} · Dead code")));
    assert_eq!(ok(&[&id.to_lowercase()]), out);
    // The same id from a subdirectory (paths change, ids do not).
    let (code, sub, _) = explain(&demo().join("services"), &[&id]);
    assert_eq!(code, 0);
    assert!(sub.contains("at pricing.py:53:5 "), "{sub}");
}

#[test]
fn dead_code_shows_evidence_effect_and_verification() {
    let id = id_of(401, &["services.pricing:legacy_discount"]);
    let out = ok(&[&id, "--llm"]);
    golden("explain-dead-llm.txt", &out);
    for want in [
        &format!("# pymapper explain {id}\n"),
        "## Source services/pricing.py:53-54\n53 > def legacy_discount(total):",
        "no-references: nothing refers to it at all",
        "call sites: none",
        "appears nowhere else in the analysed Python files",
        "mode: .: application ([project.scripts] in pyproject.toml)",
        "## Effect of deleting it",
        "## Blind spots\nnot scanned in this repository: 0 notebooks",
        "grep -rnw legacy_discount . --exclude-dir=.venv",
        "add `# pymapper: ignore[PM401]` at the end of services/pricing.py:53",
        "\nNext: ",
    ] {
        assert!(out.contains(want), "missing {want:?}:\n{out}");
    }
    // A chain: format_currency is only called by the unused format_receipt.
    let out = ok(&["utils.formatting:format_currency"]);
    assert!(out.contains("middleware.pipeline:format_receipt [certain] at middleware/pipeline.py:23: itself unused (PM401-"), "{out}");
    assert!(out.contains("import statements to remove with it (1):\n      middleware/pipeline.py:2"), "{out}");
    // Deleting format_receipt makes format_currency unused too.
    let out = ok(&["middleware.pipeline:format_receipt"]);
    assert!(out.contains("also unused once it goes (1):\n      utils.formatting:format_currency at utils/formatting.py:5 PM401-"), "{out}");
    // A module: its importers instead of callers.
    let out = ok(&[&id_of(404, &["utils.legacy_helpers"])]);
    assert!(out.contains("imports of it: none in the analysed files"), "{out}");
}

#[test]
fn complexity_lists_every_metric_and_where_it_comes_from() {
    let out = ok(&["calculate_price"]);
    golden("explain-complexity.txt", &out);
    // The symbol's four metric findings render as one block.
    assert_eq!(out.matches("Complexity and size · ").count(), 1, "{out}");
    for want in [
        "also PM102-",
        "McCabe: 14 > 10 PM101 over",
        "cognitive: 32 > 15 PM102 over",
        "nesting: 5 > 4 PM103 over",
        "lines (excluding docstrings): 30 <= 100",
        "parameters (excluding self/cls): 8 > 7 PM105 over",
        "deepest nesting: 5 levels, lines 9 > 10 > 11 > 12 > 13",
        "Split points",
        "1 direct caller (0 possible); impact: 2 affected symbols in 1 file, 0 in tests",
        "callers: 1 certain/likely, 0 possible, 0 dispatch only",
    ] {
        assert!(out.contains(want), "missing {want:?}:\n{out}");
    }
    // Any of the metric ids explains all of them.
    let by_id = ok(&[&id_of(103, &["services.pricing:calculate_price"])]);
    assert!(by_id.contains("McCabe: 14 > 10 PM101 over") && by_id.contains("also PM101-"), "{by_id}");
}

#[test]
fn cycle_lists_every_edge_with_its_kind() {
    let out = ok(&[&id_of(301, &["repo.order_repo", "models.order"]), "--llm"]);
    golden("explain-cycle-llm.txt", &out);
    for want in [
        "## Members\n2 modules: models.order, repo.order_repo",
        "models/order.py:2 models.order -> repo.order_repo (top-level): from repo.order_repo import next_order_id",
        "repo/order_repo.py:1 repo.order_repo -> models.order (top-level): from models.order import Order",
        "a real runtime cycle",
        "models/order.py:2: `next_order_id` is used only inside functions or classes (lines 7): move this import into them",
    ] {
        assert!(out.contains(want), "missing {want:?}:\n{out}");
    }
}

#[test]
fn contract_shows_the_rule_and_the_offending_import() {
    let out = ok(&[&id_of(303, &["models.order", "repo.order_repo", "models are independent of repo"])]);
    for want in [
        "\"models are independent of repo\": forbidden: models may not import repo",
        "Offending import models/order.py:2",
        "kind: top-level",
        "Other modules importing repo.order_repo (1)\n    app at app.py:7",
    ] {
        assert!(out.contains(want), "missing {want:?}:\n{out}");
    }
}

#[test]
fn duplication_shows_both_copies_and_the_differences() {
    let out = ok(&["services.pricing:receipt_lines", "--set", "analysis.dupes = true", "--llm"]);
    golden("explain-dupes-llm.txt", &out);
    // One pair, shown once from the symbol's side.
    assert_eq!(out.matches("Duplication · duplicate-code").count(), 1, "{out}");
    assert!(out.contains("## Copy services/pricing.py:39-50 (services.pricing:receipt_lines)"), "{out}");
    assert!(out.contains("## Copy services/invoice.py:1-12 (services.invoice:invoice_lines)"), "{out}");
    assert!(out.contains("2 of 12 lines differ"), "{out}");
}

#[test]
fn unused_import_shows_the_statement_and_export_status() {
    let out = ok(&["services.pricing", "--llm"]);
    for want in ["1 > import json", "import kind: top-level", "__all__: not defined", "`json` appears nowhere else in the file", "grep -nw json services/pricing.py"] {
        assert!(out.contains(want), "missing {want:?}:\n{out}");
    }
}

#[test]
fn bad_targets_exit_2() {
    // Ambiguous symbol: candidates on stdout, error on stderr.
    let (code, out, err) = explain(&demo(), &["__init__"]);
    assert_eq!(code, 2);
    assert_eq!(out.lines().count(), 3, "{out}");
    assert!(err.contains("`__init__` is ambiguous: 3 matches"), "{err}");
    // Unknown id and unknown symbol.
    let (code, _, err) = explain(&demo(), &["PM401-0000"]);
    assert_eq!(code, 2);
    assert!(err.contains("no finding has this id"), "{err}");
    let (code, _, err) = explain(&demo(), &["no_such_symbol_xyz"]);
    assert_eq!(code, 2);
    assert!(err.contains("neither a finding id"), "{err}");
    // Machine formats are not explain's.
    let (code, _, err) = explain(&demo(), &["calculate_price", "--format", "json"]);
    assert_eq!(code, 2);
    assert!(err.contains("writes the human and llm formats"), "{err}");
}
