//! `incremental`: single-file edit latency of `pymapper_core::incremental::Program::update`.
//!
//! The corpus is copied into a temporary directory (phase `copy`; corpora over
//! [`MAX_COPY_BYTES`] are not copied and their files are re-extracted unchanged instead, counter
//! `inplace`), loaded and resolved with calls (phases `load.*`, `resolve.*`), then a set of
//! files is edited one at a time: a function is appended, the program updated, the file
//! restored and the program updated again; the same with a method added to the first class
//! (the method-index scan) and with a module-level import appended (readers re-resolved).
//! The files are spread over the size distribution plus the stdlib's most imported modules
//! when present. Phase `incremental.update` is the sum of all updates; counters give
//! per-update medians, overall and per kind of edit.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Context;
use pymapper_core::incremental::{Program, UpdateStats};
use pymapper_core::index::Inputs;
use pymapper_core::resolve::Attribution;
use pymapper_core::roots::Origin;
use pymapper_core::timing;
use pymapper_core::walk::PathList;

use super::{Counters, ReadStats, corpus_options, walk_corpus};

/// Corpora larger than this are edited in place without changing contents.
const MAX_COPY_BYTES: u64 = 200_000_000;
/// Rounds over the edited files (medians over all of them).
const ROUNDS: usize = 3;
/// Modules worth editing when present: imported by most of the stdlib.
const HOT: &[&str] = &["typing.py", "os.py", "collections/__init__.py", "functools.py", "re/__init__.py"];

/// A temporary copy of the corpus, removed on drop.
struct Copy(PathBuf);

impl Drop for Copy {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// One kind of edit; every edit is followed by an update, a restore and another update.
#[derive(Clone, Copy)]
enum Edit {
    /// Append a top-level function.
    Append,
    /// Add a method to the first top-level class (new method name: the method index changes,
    /// so every call row of the program is scanned for it). Files without a class are skipped.
    Method,
    /// Append a module-level import (the file's namespace and imports change: its readers are
    /// re-resolved).
    Import,
}

impl Edit {
    fn apply(self, text: &[u8], round: usize) -> Option<Vec<u8>> {
        match self {
            Edit::Append => {
                let mut t = text.to_vec();
                t.extend_from_slice(format!("\n\ndef _pymapper_bench_{round}():\n    return {round}\n").as_bytes());
                Some(t)
            }
            Edit::Import => {
                let mut t = text.to_vec();
                t.extend_from_slice(format!("\n\nimport os as _pymapper_bench_os_{round}\n").as_bytes());
                Some(t)
            }
            Edit::Method => {
                // After a one-line `class X...:` header at column 0, indented like the next
                // non-blank line.
                let lines: Vec<&[u8]> = text.split_inclusive(|&c| c == b'\n').collect();
                let at = lines.iter().position(|l| l.starts_with(b"class ") && l.trim_ascii_end().ends_with(b":"))?;
                let body = lines[at + 1..].iter().find(|l| !l.trim_ascii().is_empty())?;
                let ind = &body[..body.len() - body.trim_ascii_start().len()];
                if ind.is_empty() {
                    return None;
                }
                let ind = String::from_utf8_lossy(ind);
                let mut t: Vec<u8> = lines[..=at].concat();
                t.extend_from_slice(format!("{ind}def _pymapper_bench_m{round}(self):\n{ind}    return {round}\n").as_bytes());
                t.extend_from_slice(&lines[at + 1..].concat());
                Some(t)
            }
        }
    }
}

fn median(v: &mut [Duration]) -> u64 {
    if v.is_empty() {
        return 0;
    }
    v.sort_unstable();
    v[v.len() / 2].as_micros() as u64
}

fn median_u32(v: impl Iterator<Item = u32>) -> u64 {
    let mut v: Vec<u32> = v.collect();
    v.sort_unstable();
    v.get(v.len() / 2).map_or(0, |&x| u64::from(x))
}

pub(super) fn incremental_target(root: &Path) -> anyhow::Result<(ReadStats, Counters)> {
    let (files, walk_errors, _) = walk_corpus(root, &corpus_options())?;
    let total: u64 = (0..files.len()).map(|i| fs::metadata(files.get(i)).map_or(0, |m| m.len())).sum();
    let inplace = total > MAX_COPY_BYTES;
    let (base, copy) = if inplace {
        (root.to_path_buf(), None)
    } else {
        let dir = std::env::temp_dir().join(format!("pymapper-bench-incr-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let copy = Copy(dir.clone());
        timing::time("copy", || -> anyhow::Result<()> {
            for p in files.iter() {
                let dst = dir.join(p.strip_prefix(root).context("file outside the corpus")?);
                fs::create_dir_all(dst.parent().context("no parent")?)?;
                fs::copy(p, &dst).with_context(|| format!("copying {}", p.display()))?;
            }
            Ok(())
        })?;
        (dir, Some(copy))
    };
    let mut paths = PathList::default();
    for p in files.iter() {
        let rel = p.strip_prefix(root).context("file outside the corpus")?;
        paths.push(base.join(rel).as_os_str().as_encoded_bytes()).map_err(|e| anyhow::anyhow!("{e}"))?;
    }
    drop(files);
    let inputs = |paths: &PathList| Inputs::from_root_dir(&base, paths, Origin::FirstParty, b"corpus");
    let mut prog = Program::load(inputs(&paths), Attribution::new(), true);
    let s = *prog.index().stats();
    let heap_before = prog.heap_bytes();
    let deps = prog.dependency_entries();

    // Files to edit: quantiles of the size distribution, then the hot modules.
    let mut by_size: Vec<(u64, usize)> =
        (0..paths.len()).map(|i| (fs::metadata(paths.get(i)).map_or(0, |m| m.len()), i)).collect();
    by_size.sort_unstable();
    let mut picks: Vec<usize> = Vec::new();
    if !by_size.is_empty() {
        for q in [10, 30, 50, 70, 90, 100] {
            picks.push(by_size[((by_size.len() - 1) * q / 100).min(by_size.len() - 1)].1);
        }
    }
    let mut hot = Vec::new();
    for h in HOT {
        if let Some(i) = (0..paths.len()).find(|&i| paths.get(i).strip_prefix(&base).is_ok_and(|r| r == Path::new(h))) {
            picks.push(i);
            hot.push(i);
        }
    }
    picks.sort_unstable();
    picks.dedup();

    let was = timing::is_enabled();
    timing::enable(false);
    let mut all: Vec<UpdateStats> = Vec::new();
    let mut by_kind: [Vec<Duration>; 3] = Default::default();
    let mut hot_totals = Vec::new();
    for round in 0..ROUNDS {
        for &i in &picks {
            let p = paths.get(i).to_path_buf();
            let original = fs::read(&p).with_context(|| format!("reading {}", p.display()))?;
            for (k, kind) in [Edit::Append, Edit::Method, Edit::Import].into_iter().enumerate() {
                // In place: one unchanged re-extraction per file and round (no edit kinds).
                let edited = match (inplace, k) {
                    (true, 0) => Some(original.clone()),
                    (true, _) => None,
                    (false, _) => kind.apply(&original, round),
                };
                let Some(edited) = edited else { continue };
                for text in [&edited, &original] {
                    if !inplace {
                        fs::write(&p, text)?;
                    }
                    let (_, st) = prog.update(inputs(&paths), &[&p]);
                    if hot.contains(&i) {
                        hot_totals.push(st.total);
                    }
                    by_kind[k].push(st.total);
                    all.push(st);
                }
            }
        }
    }
    timing::enable(was);
    let sum: Duration = all.iter().map(|s| s.total).sum();
    timing::record("incremental.update", sum);
    let col = |f: fn(&UpdateStats) -> Duration| -> Vec<Duration> { all.iter().map(f).collect() };
    let mut totals = col(|s| s.total);
    let best = totals.iter().min().map_or(0, |d| d.as_micros() as u64);
    let worst = totals.iter().max().map_or(0, |d| d.as_micros() as u64);
    let counters = vec![
        ("loc".to_owned(), s.lines),
        ("inplace".to_owned(), u64::from(inplace)),
        ("updates".to_owned(), all.len() as u64),
        ("update_median_us".to_owned(), median(&mut totals)),
        ("update_best_us".to_owned(), best),
        ("update_worst_us".to_owned(), worst),
        ("hot_median_us".to_owned(), median(&mut hot_totals)),
        ("append_median_us".to_owned(), median(&mut by_kind[0])),
        ("method_median_us".to_owned(), median(&mut by_kind[1])),
        ("import_median_us".to_owned(), median(&mut by_kind[2])),
        ("plan_us".to_owned(), median(&mut col(|s| s.plan))),
        ("extract_us".to_owned(), median(&mut col(|s| s.extract))),
        ("patch_us".to_owned(), median(&mut col(|s| s.patch))),
        ("resolve_imports_us".to_owned(), median(&mut col(|s| s.resolve_imports))),
        ("resolve_calls_us".to_owned(), median(&mut col(|s| s.resolve_calls))),
        ("diff_us".to_owned(), median(&mut col(|s| s.diff))),
        ("reresolved_imports_median".to_owned(), median_u32(all.iter().map(|s| s.reresolved_imports))),
        ("reresolved_calls_median".to_owned(), median_u32(all.iter().map(|s| s.reresolved_calls))),
        ("deps_imports".to_owned(), deps.0 as u64),
        ("deps_calls".to_owned(), deps.1 as u64),
        ("tracking_kb".to_owned(), prog.tracking_bytes() as u64 / 1000),
        ("heap_kb_before".to_owned(), heap_before as u64 / 1000),
        ("heap_kb_after".to_owned(), prog.heap_bytes() as u64 / 1000),
    ];
    drop(prog);
    drop(copy);
    let stats = ReadStats { files: s.files, bytes: s.bytes, errors: walk_errors };
    Ok((stats, counters))
}
