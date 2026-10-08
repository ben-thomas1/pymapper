//! Repository layout invariants that later steps rely on, plus xtask stub behaviour.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).parent().expect("xtask has a parent dir").to_path_buf()
}

/// Recursively collects regular files under `dir` (no symlink following).
fn files_under(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).unwrap_or_else(|e| panic!("read_dir {}: {e}", dir.display())) {
        let entry = entry.expect("dir entry");
        let ty = entry.file_type().expect("file type");
        if ty.is_dir() {
            files_under(&entry.path(), out);
        } else if ty.is_file() {
            out.push(entry.path());
        }
    }
}

#[test]
fn toolchain_is_pinned_to_an_exact_stable_version() {
    let text = fs::read_to_string(repo_root().join("rust-toolchain.toml")).expect("rust-toolchain.toml");
    let channel = text
        .lines()
        .find_map(|l| l.trim().strip_prefix("channel"))
        .and_then(|rest| rest.trim().strip_prefix('='))
        .map(|v| v.trim().trim_matches('"'))
        .expect("channel key");
    let parts: Vec<&str> = channel.split('.').collect();
    assert_eq!(parts.len(), 3, "channel must be an exact x.y.z version, got {channel:?}");
    for p in parts {
        assert!(!p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()), "bad channel {channel:?}");
    }
}

#[test]
fn design_doc_is_present() {
    let text = fs::read_to_string(repo_root().join("docs/design.md")).expect("docs/design.md");
    assert!(text.starts_with("# pymapper rewrite - design (source of truth)"));
    for section in ["## Goals", "## Dependency policy", "## Performance rules", "## Front end", "## GUI"] {
        assert!(text.contains(section), "design.md missing {section}");
    }
}

#[test]
fn corpus_contains_only_python_files() {
    let root = repo_root().join("tests/corpus");
    for (sub, min) in [("fixtures", 10), ("sample_python", 18)] {
        let mut files = Vec::new();
        files_under(&root.join(sub), &mut files);
        assert!(files.len() >= min, "{sub}: expected >= {min} files, found {}", files.len());
        for f in &files {
            assert_eq!(f.extension().and_then(|e| e.to_str()), Some("py"), "non-Python file {}", f.display());
        }
    }
}

#[test]
fn xtask_stubs_exit_2() {
    let name = "diff-ruff";
    let out = Command::new(env!("CARGO_BIN_EXE_xtask")).arg(name).output().expect("spawn xtask");
    assert_eq!(out.status.code(), Some(2), "{name}");
    assert!(String::from_utf8_lossy(&out.stderr).contains("not implemented"), "{name}");
}
