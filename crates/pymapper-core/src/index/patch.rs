//! Patching a loaded [`Index`] with re-extracted files (incremental updates, see
//! [`crate::incremental`]).
//!
//! ```text
//!   old Index ──────────────┐   files kept: rows stay where they are
//!   fresh Index (re-read) ──┼─> interner: fresh strings merged into the old one (no
//!   new Inputs + sources    │   renumbering: every old Sym stays valid), fresh rows remapped
//!                           ├─> facts + metrics: one in-place splice pass per column
//!                           │   (removed runs dropped, fresh runs inserted, rows between
//!                           │   moved once), headers rebased by running sums
//!                           └─> hierarchy + lookup tables rebuilt from the patched tables
//! ```
//!
//! The result is the index [`super::load`] builds for the new inputs (same rows, nodes and
//! qualified ids; only `Sym` values differ, which never matters). The fact store stays compact
//! and in FileId order, so every consumer's invariants hold: removed rows are compacted
//! eagerly by the splice. Strings of removed rows stay in the interner as tombstones until
//! [`Index::compact_strings`] drops them.

use std::time::{Duration, Instant};

use super::{Index, Inputs, LoadStats, ReadError, build};
use crate::extract::facts::{FileFacts, Run, TableBases};
use crate::extract::splice::Splice;
use crate::index::FileId;
use crate::intern::{Interner, Sym};
use crate::timing;

/// Where the facts of a file of the new inputs come from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Src {
    /// Unchanged: file `f` of the old index.
    Old(FileId),
    /// Re-extracted: file `f` of the fresh index.
    Fresh(FileId),
}

/// `ff` with every run restarted at the running table lengths `at` (which advance).
fn rebase(ff: &FileFacts, at: &mut TableBases) -> FileFacts {
    let mut out = *ff;
    macro_rules! runs {
        ($($t:ident),+) => {$(
            out.$t = Run { start: at.$t, len: ff.$t.len };
            at.$t += ff.$t.len;
        )+};
    }
    runs!(defs, decorators, bases, params, imports, calls, bindings, decls, names, exports, evidence, diags, suppressions);
    out
}

/// A header whose runs are empty and start at `at` (an insertion point).
fn empty_at(at: &TableBases) -> FileFacts {
    let mut out = FileFacts::default();
    macro_rules! runs {
        ($($t:ident),+) => {$( out.$t = Run { start: at.$t, len: 0 }; )+};
    }
    runs!(defs, decorators, bases, params, imports, calls, bindings, decls, names, exports, evidence, diags, suppressions);
    out
}

/// Runs where the rows of a file at position `ff` start (its own runs' starts).
fn starts(ff: &FileFacts) -> TableBases {
    let mut b = TableBases::default();
    macro_rules! runs {
        ($($t:ident),+) => {$( b.$t = ff.$t.start; )+};
    }
    runs!(defs, decorators, bases, params, imports, calls, bindings, decls, names, exports, evidence, diags, suppressions);
    b
}

/// Metrics rows of file `f` with header `ff`: one per def row except `<module>`, files in
/// FileId order (so file `f`'s rows start at `defs.start - f`).
#[inline]
fn metrics_run(ff: &FileFacts, f: FileId) -> (u32, u32) {
    (ff.defs.start - f, ff.defs.len.saturating_sub(1))
}

impl Index {
    /// The index of `inputs`, built from this one and `fresh`, an index of the re-extracted
    /// files: `src[i]` says where file `i` of `inputs` comes from. `Old` files must appear in
    /// increasing order (convert out-of-order files to `Fresh` first); old files not referenced
    /// are removed. See the module docs.
    ///
    /// # Panics
    /// If `src` does not match `inputs`, or `Old` entries are not increasing.
    pub(crate) fn patch(self, inputs: Inputs, src: &[Src], mut fresh: Index) -> Index {
        assert_eq!(src.len(), inputs.len(), "one source per input file");
        let t0 = Instant::now();
        let Index { mut strings, facts: mut store, mut metrics, files, read_errors, stats, .. } = self;
        let old_ff = files.facts;
        let old_n = old_ff.len();

        // Fresh strings into the old interner: old Syms stay valid.
        let remap = timing::time("incremental.intern", || strings.merge(&fresh.strings));
        if !remap.is_identity() {
            for ff in &fresh.files.facts {
                fresh.facts.map_syms(ff, |s| remap.get(s));
            }
        }
        drop(remap);

        // Splice plan, in position order, and the new headers (before rebasing).
        let end = store.bases();
        let anchor = |c: usize| if c < old_n { starts(&old_ff[c]) } else { end };
        let metrics_anchor = |c: usize| if c < old_n { metrics_run(&old_ff[c], c as FileId).0 } else { metrics.len() as u32 };
        let mut edits: Vec<(FileFacts, FileFacts)> = Vec::new();
        let mut mplan: Vec<Splice> = Vec::new();
        let mut headers: Vec<FileFacts> = Vec::with_capacity(src.len());
        let mut old_to_new = vec![u32::MAX; old_n];
        let mut c = 0usize;
        let none = FileFacts::default();
        let remove = |g: usize, edits: &mut Vec<(FileFacts, FileFacts)>, mplan: &mut Vec<Splice>| {
            edits.push((old_ff[g], none));
            let (at, old) = metrics_run(&old_ff[g], g as FileId);
            mplan.push(Splice { at, old, src: 0, new: 0 });
        };
        for (i, s) in src.iter().enumerate() {
            match *s {
                Src::Old(f) => {
                    let f = f as usize;
                    assert!(f >= c && f < old_n, "old files out of order");
                    for g in c..f {
                        remove(g, &mut edits, &mut mplan);
                    }
                    c = f + 1;
                    headers.push(old_ff[f]);
                    old_to_new[f] = i as u32;
                }
                Src::Fresh(j) => {
                    let nf = fresh.files.facts[j as usize];
                    edits.push((empty_at(&anchor(c)), nf));
                    let (s0, n0) = metrics_run(&nf, j);
                    mplan.push(Splice { at: metrics_anchor(c), old: 0, src: s0, new: n0 });
                    headers.push(nf);
                }
            }
        }
        for g in c..old_n {
            remove(g, &mut edits, &mut mplan);
        }

        timing::time("incremental.splice", || {
            let mut plan = Vec::new();
            store.splice(&edits, &fresh.facts, &mut plan);
            metrics.splice(&mplan, &fresh.metrics);
        });
        drop(fresh.facts);
        drop(fresh.metrics);
        let mut at = TableBases::default();
        for h in &mut headers {
            *h = rebase(h, &mut at);
        }
        debug_assert_eq!(at, store.bases());

        let mut errors: Vec<ReadError> = Vec::new();
        for e in &read_errors {
            let nf = old_to_new[e.file as usize];
            if nf != u32::MAX {
                errors.push(ReadError { file: nf, kind: e.kind });
            }
        }
        if !fresh.read_errors.is_empty() {
            let mut fresh_to_new = vec![u32::MAX; fresh.files.facts.len()];
            for (i, s) in src.iter().enumerate() {
                if let Src::Fresh(j) = *s {
                    fresh_to_new[j as usize] = i as u32;
                }
            }
            for e in &fresh.read_errors {
                let nf = fresh_to_new[e.file as usize];
                if nf != u32::MAX {
                    errors.push(ReadError { file: nf, kind: e.kind });
                }
            }
        }
        errors.sort_unstable_by_key(|e| e.file);

        let lines = headers.iter().map(|ff| u64::from(ff.lines)).sum();
        let st = LoadStats {
            files: inputs.len() as u64,
            bytes: fresh.stats.bytes,
            lines,
            extract: fresh.stats.extract,
            merge: t0.elapsed(),
            build: Duration::ZERO,
        };
        let _ = stats;
        let t1 = Instant::now();
        let mut ix = timing::time("incremental.build", || build::build(inputs, strings, store, metrics, headers, errors, st));
        ix.stats.build = t1.elapsed();
        ix
    }

    /// Whether `inputs` lists exactly this index's paths, in the same order.
    pub(crate) fn same_paths(&self, inputs: &Inputs) -> bool {
        self.paths == inputs.paths
    }

    /// Drops strings no row, node or module name uses (tombstones of removed rows) and
    /// renumbers the rest in their old order. Every `Sym` held outside the index (resolution
    /// tables, callers) is invalidated. Returns `(live, dead)` string counts. O(rows).
    pub(crate) fn compact_strings(&mut self) -> (usize, usize) {
        let n = self.strings.len();
        let mut live = vec![false; n];
        let files = &self.files;
        for ff in &files.facts {
            self.facts.map_syms(ff, |s| {
                live[s.index()] = true;
                s
            });
        }
        for &s in self.nodes.name.iter().chain(&files.module) {
            live[s.index()] = true;
        }
        for &(s, _) in self.modules.keys() {
            live[s.index()] = true;
        }
        let kept = live.iter().filter(|&&l| l).count();
        if kept == n {
            return (n, 0);
        }
        let mut next = Interner::new();
        next.reserve(kept, self.strings.string_bytes());
        let mut map = vec![Sym::from_index(0); n];
        for (s, bytes) in self.strings.iter() {
            if live[s.index()] {
                map[s.index()] = next.intern(bytes);
            }
        }
        drop(live);
        let Index { strings, facts, files, nodes, modules, .. } = self;
        *strings = next;
        for ff in &files.facts {
            facts.map_syms(ff, |s| map[s.index()]);
        }
        for s in nodes.name.iter_mut().chain(files.module.iter_mut()) {
            *s = map[s.index()];
        }
        let old = std::mem::take(modules);
        modules.reserve(old.len());
        for ((s, d), v) in old {
            modules.insert((map[s.index()], d), v);
        }
        (kept, n - kept)
    }
}
