//! `watch`: edit-to-update latency through the real file watcher
//! (`pymapper_core::watch`) and `incremental::Program::update`, as `pymapper check --watch`
//! and `pymapper-gui --watch` run it.
//!
//! The corpus is copied into a temporary directory (phase `copy`; corpora over
//! [`MAX_COPY_BYTES`] are refused: the watcher needs real writes and the corpus itself is never
//! written), loaded and resolved with calls, and watched with the default debounce. Files
//! across the size distribution are edited (a function appended) and restored, one write at a
//! time; each write is timed until its update is applied:
//!
//! ```text
//!   write ──> first event (OS delivery) ──> batch closes (debounce) ──> classify + update
//!   |<- event_us ->|<----- debounce_us ----->|<-------- update_us -------->|
//!   |<------------------------------- total_us --------------------------->|
//! ```
//!
//! `update_us` includes classifying the batch (`classify_median_us`) and rebuilding the inputs
//! from the index (`inputs_us`, measured once). Stray events of a write (FSEvents may report a
//! file twice) are drained before the next one. Phase `watch.edits` is the sum of all totals.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::Context;
use pymapper_core::incremental::Program;
use pymapper_core::index::Inputs;
use pymapper_core::resolve::Attribution;
use pymapper_core::roots::Origin;
use pymapper_core::timing;
use pymapper_core::walk::PathList;
use pymapper_core::watch::{self, Batch, Wait, WatchOptions, Watcher};

use super::{Counters, ReadStats, corpus_options, walk_corpus};

/// Corpora larger than this are not copied (and never edited in place).
const MAX_COPY_BYTES: u64 = 1_000_000_000;
/// Rounds over the edited files.
const ROUNDS: usize = 3;
/// How long to wait for the batch of one write before giving up.
const EVENT_TIMEOUT: Duration = Duration::from_secs(10);
/// Quiet time that counts as "no stray events left" between writes.
const DRAIN: Duration = Duration::from_millis(100);

/// A temporary copy of the corpus, removed on drop.
struct Copy(PathBuf);

impl Drop for Copy {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn median(v: &mut [Duration]) -> u64 {
    if v.is_empty() {
        return 0;
    }
    v.sort_unstable();
    v[v.len() / 2].as_micros() as u64
}

fn pct(v: &mut [Duration], p: usize) -> u64 {
    if v.is_empty() {
        return 0;
    }
    v.sort_unstable();
    v[((v.len() - 1) * p / 100).min(v.len() - 1)].as_micros() as u64
}

pub(super) fn watch_target(root: &Path) -> anyhow::Result<(ReadStats, Counters)> {
    let (files, walk_errors, _) = walk_corpus(root, &corpus_options())?;
    let total: u64 = (0..files.len()).map(|i| fs::metadata(files.get(i)).map_or(0, |m| m.len())).sum();
    anyhow::ensure!(total <= MAX_COPY_BYTES, "corpus too large to copy for the watch benchmark ({total} bytes)");
    let dir = std::env::temp_dir().join(format!("pymapper-bench-watch-{}", std::process::id()));
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
    let mut paths = PathList::default();
    for p in files.iter() {
        let rel = p.strip_prefix(root).context("file outside the corpus")?;
        paths.push(dir.join(rel).as_os_str().as_encoded_bytes()).map_err(|e| anyhow::anyhow!("{e}"))?;
    }
    drop(files);
    let inputs = |paths: &PathList| Inputs::from_root_dir(&dir, paths, Origin::FirstParty, b"corpus");
    let mut prog = Program::load(inputs(&paths), Attribution::new(), true);
    let s = *prog.index().stats();
    let opts = WatchOptions::default();
    let watcher = Watcher::new(&[&dir], opts).context("starting the watcher")?;
    let mut batch = Batch::new();
    drain(&watcher, &mut batch);

    let mut by_size: Vec<(u64, usize)> = (0..paths.len()).map(|i| (fs::metadata(paths.get(i)).map_or(0, |m| m.len()), i)).collect();
    by_size.sort_unstable();
    let mut picks: Vec<usize> = Vec::new();
    if !by_size.is_empty() {
        for q in [10, 30, 50, 70, 90, 100] {
            picks.push(by_size[((by_size.len() - 1) * q / 100).min(by_size.len() - 1)].1);
        }
    }
    picks.sort_unstable();
    picks.dedup();

    let was = timing::is_enabled();
    timing::enable(false);
    let (mut event, mut debounce, mut update, mut totals) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    let mut classify = Vec::new();
    // What every content-only update pays on top of `Program::update`: the inputs rebuilt
    // from the index.
    let t = Instant::now();
    drop(watch::inputs_of(prog.index()));
    let inputs_us = t.elapsed().as_micros() as u64;
    let mut rewalks = 0u64;
    let mut missed = 0u64;
    for round in 0..ROUNDS {
        for &i in &picks {
            let p = paths.get(i).to_path_buf();
            let original = fs::read(&p).with_context(|| format!("reading {}", p.display()))?;
            let mut edited = original.clone();
            edited.extend_from_slice(format!("\n\ndef _pymapper_watch_bench_{round}():\n    return {round}\n").as_bytes());
            for text in [&edited, &original] {
                let t0 = Instant::now();
                fs::write(&p, text)?;
                let Some(t_batch) = wait_for(&watcher, &mut batch, &p, t0) else {
                    missed += 1;
                    continue;
                };
                let first = batch.first_event().unwrap_or(t_batch).max(t0);
                let changes = watch::classify(&batch, prog.index(), &dir);
                classify.push(t_batch.elapsed());
                rewalks += u64::from(changes.rewalk);
                let _ = watch::update(&mut prog, &changes, || {
                    let w = pymapper_core::walk::walk(&[&dir], &corpus_options());
                    inputs(&w.files)
                });
                let done = Instant::now();
                event.push(first - t0);
                debounce.push(t_batch - first);
                update.push(done - t_batch);
                totals.push(done - t0);
                drain(&watcher, &mut batch);
            }
        }
    }
    timing::enable(was);
    timing::record("watch.edits", totals.iter().sum());
    let counters = vec![
        ("loc".to_owned(), s.lines),
        ("edits".to_owned(), totals.len() as u64),
        ("missed".to_owned(), missed),
        ("rewalks".to_owned(), rewalks),
        ("quiet_ms".to_owned(), opts.quiet.as_millis() as u64),
        ("total_median_us".to_owned(), median(&mut totals)),
        ("total_p90_us".to_owned(), pct(&mut totals, 90)),
        ("total_worst_us".to_owned(), pct(&mut totals, 100)),
        ("event_median_us".to_owned(), median(&mut event)),
        ("event_worst_us".to_owned(), pct(&mut event, 100)),
        ("debounce_median_us".to_owned(), median(&mut debounce)),
        ("update_median_us".to_owned(), median(&mut update)),
        ("classify_median_us".to_owned(), median(&mut classify)),
        ("inputs_us".to_owned(), inputs_us),
        ("update_worst_us".to_owned(), pct(&mut update, 100)),
    ];
    drop(watcher);
    drop(prog);
    drop(copy);
    let stats = ReadStats { files: s.files, bytes: s.bytes, errors: walk_errors };
    Ok((stats, counters))
}

/// Waits for the batch holding `p` (written at `t0`); returns when it closed.
fn wait_for(w: &Watcher, b: &mut Batch, p: &Path, t0: Instant) -> Option<Instant> {
    loop {
        let left = EVENT_TIMEOUT.checked_sub(t0.elapsed())?;
        if w.wait(b, Some(left)) == Wait::Ready && (b.rescan() || b.paths().any(|q| q == p)) {
            return Some(Instant::now());
        }
    }
}

/// Discards batches until the watcher has been quiet for [`DRAIN`].
fn drain(w: &Watcher, b: &mut Batch) {
    w.flush();
    while w.wait(b, Some(DRAIN)) == Wait::Ready {}
}

