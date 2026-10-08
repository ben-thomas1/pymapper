//! Exact FileFacts of every file in tests/corpus, compared with reviewed golden renderings in
//! `tests/facts_corpus/<path>.facts` (tests/corpus itself holds only Python files).
//!
//! After an intended change, regenerate with `PYMAPPER_BLESS=1 cargo test --test facts_corpus`
//! and review the diff of the golden files.

use std::path::{Path, PathBuf};

use pymapper_core::extract::facts::{FactStore, SpanStrs, extract, render};
use pymapper_core::walk::{WalkOptions, walk};

fn manifest() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

#[test]
fn corpus_facts_match_golden_files() {
    let corpus = manifest().join("../../tests/corpus");
    let golden = manifest().join("tests/facts_corpus");
    let bless = std::env::var_os("PYMAPPER_BLESS").is_some();
    let files = walk(&[&corpus], &WalkOptions::everything()).files;
    assert!(files.len() >= 28, "corpus not found at {}", corpus.display());

    // One store for all files, like a worker thread: also checks file-relative indexing.
    let mut st = FactStore::new();
    let mut spans = SpanStrs::new();
    let mut failures = Vec::new();
    let mut seen = Vec::new();
    for path in files.iter() {
        let rel = path.strip_prefix(&corpus).unwrap_or(path);
        let src = std::fs::read(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        let ff = extract(&src, &mut st, &mut spans);
        let got = render(st.file(&ff), |s| spans.text(&src, s));
        let want_path = golden.join(rel).with_extension("facts");
        seen.push(want_path.clone());
        if bless {
            std::fs::create_dir_all(want_path.parent().unwrap_or(Path::new("."))).unwrap_or_default();
            std::fs::write(&want_path, &got).unwrap_or_else(|e| panic!("{}: {e}", want_path.display()));
            continue;
        }
        let want = std::fs::read_to_string(&want_path).unwrap_or_default();
        if got != want {
            failures.push(format!("{}:\n--- got ---\n{got}--- want ---\n{want}", rel.display()));
        }
    }
    // No stale golden files.
    let mut stale = Vec::new();
    collect(&golden, &mut stale);
    stale.retain(|p| !seen.contains(p));
    assert!(stale.is_empty(), "golden files without a corpus file: {stale:?}");
    assert!(failures.is_empty(), "{} files differ:\n{}", failures.len(), failures.join("\n"));
}

fn collect(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            collect(&p, out);
        } else {
            out.push(p);
        }
    }
}
