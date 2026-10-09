//! Incremental updates (design: Semantics, Incremental API): changed files -> re-extract ->
//! patch the index -> re-resolve -> index diff.
//!
//! A [`Program`] holds a loaded [`Index`] with its resolved imports and (optionally) calls.
//! [`Program::update`] takes the new [`Inputs`] (the files to load now: callers rebuild them
//! from a walk or patch them from file events; building inputs does no I/O beyond the walk)
//! and the paths whose contents changed, and brings the program to exactly the state a full
//! load of the new inputs would give:
//!
//! ```text
//!   new Inputs + changed paths
//!     └─> plan         match files by path: kept (facts reused), re-extracted (changed, new,
//!     │                or out of order), removed
//!     └─> snapshot     the little the diff needs from the old index (nodes of dropped files)
//!     └─> extract      only the re-extracted files, with the normal parallel loader
//!     └─> patch        fresh strings merged into the interner (old Syms stay valid); facts and
//!     │                metrics spliced in place (one memmove pass per column); hierarchy and
//!     │                lookup tables rebuilt
//!     └─> compaction   the interner keeps strings of removed rows as tombstones; once it has
//!     │                doubled since the last compaction, live strings are marked and the
//!     │                interner is rebuilt (amortised O(1) per added string)
//!     └─> resolve      imports and calls against the patched index
//!     └─> diff         node correspondence, added / removed / changed nodes, import, call and
//!                      override edge deltas (see [`IndexDiff`])
//! ```
//!
//! # Equivalence
//!
//! The patched index has the same files, rows, nodes and qualified ids as a fresh load of the
//! same inputs (only `Sym` values differ, which never matters), and resolution is a function of
//! the index, so every result equals the full rebuild's. The property tests check this with
//! random edits (changes, additions, removals) on corpora.
//!
//! # Cost
//!
//! Reading and extracting, the dominant cost of a load, is paid only for re-extracted files.
//! The rest is linear in the size of the index with small constants: the splice moves the rows
//! behind the first changed file once (memmove), the hierarchy rebuild is a sort plus one pass
//! over the nodes, and the diff translates old nodes by offset. Resolution currently reruns
//! for the whole program (a few ms on the stdlib; see `docs/benchmarks.md`).

mod diff;
mod edges;
mod plan;

#[cfg(test)]
mod tests;

pub use diff::{EdgeDelta, EdgeLabel, IndexDiff};

use std::path::Path;
use std::time::{Duration, Instant};

use crate::index::{self, Index, Inputs, NONE};
use crate::resolve::{Attribution, Imports, ResolvedCalls, resolve_calls, resolve_imports};
use crate::timing;

/// Interners smaller than this are never compacted (the mark pass would cost more than the
/// memory it frees).
const MIN_COMPACT_STRINGS: usize = 1 << 16;

/// Counters and phase times of one [`Program::update`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct UpdateStats {
    pub files: u32,
    pub reextracted: u32,
    pub added: u32,
    pub removed: u32,
    /// Bytes read by the re-extraction.
    pub bytes: u64,
    /// Interner strings dropped by a compaction (0: no compaction).
    pub strings_dropped: u32,
    pub plan: Duration,
    pub extract: Duration,
    pub patch: Duration,
    pub resolve_imports: Duration,
    pub resolve_calls: Duration,
    pub diff: Duration,
    pub total: Duration,
}

/// A loaded program kept up to date by [`Program::update`] (see the module docs).
pub struct Program {
    index: Index,
    imports: Imports,
    calls: Option<ResolvedCalls>,
    attr: Attribution,
    /// Interner size after the last compaction (or the load).
    strings_base: usize,
}

impl Program {
    /// Loads `inputs`, resolves imports against `attr` and, with `calls`, call sites.
    pub fn load(inputs: Inputs, attr: Attribution, calls: bool) -> Program {
        let index = index::load(inputs);
        Program::from_parts(index, attr, calls)
    }

    /// A program from an already loaded index (resolves it).
    pub fn from_parts(index: Index, attr: Attribution, calls: bool) -> Program {
        let imports = resolve_imports(&index, &attr);
        let calls = calls.then(|| resolve_calls(&index, &imports));
        let strings_base = index.strings().len();
        Program { index, imports, calls, attr, strings_base }
    }

    #[inline]
    pub fn index(&self) -> &Index {
        &self.index
    }

    #[inline]
    pub fn imports(&self) -> &Imports {
        &self.imports
    }

    /// Resolved calls, when the program resolves calls.
    #[inline]
    pub fn calls(&self) -> Option<&ResolvedCalls> {
        self.calls.as_ref()
    }

    #[inline]
    pub fn attribution(&self) -> &Attribution {
        &self.attr
    }

    pub fn into_parts(self) -> (Index, Imports, Option<ResolvedCalls>) {
        (self.index, self.imports, self.calls)
    }

    /// Heap bytes of the index and the resolution tables.
    pub fn heap_bytes(&self) -> usize {
        self.index.heap_bytes()
            + self.imports.heap_bytes()
            + self.calls.as_ref().map_or(0, ResolvedCalls::heap_bytes)
            + self.attr.heap_bytes()
    }

    /// Brings the program to the state of a full load of `inputs`, re-extracting only the
    /// files listed in `changed` (paths as in the inputs), the files not loaded before and
    /// files whose order changed. Files of the old inputs missing from `inputs` are removed.
    /// Files whose contents changed but are not listed keep their old facts.
    ///
    /// Returns what changed (see [`IndexDiff`]). Phases are recorded with [`timing`] as
    /// `incremental.*`.
    pub fn update<P: AsRef<Path>>(&mut self, inputs: Inputs, changed: &[P]) -> (IndexDiff, UpdateStats) {
        let t0 = Instant::now();
        let mut st = UpdateStats { files: inputs.len() as u32, ..UpdateStats::default() };
        let plan = timing::time("incremental.plan", || plan::plan(&self.index, &inputs, changed));
        if plan.noop {
            st.plan = t0.elapsed();
            st.total = st.plan;
            let node_map = (0..self.index.node_count()).collect();
            return (IndexDiff { node_map, ..IndexDiff::default() }, st);
        }
        let snap = timing::time("incremental.snapshot", || diff::OldSnap::take(&self.index, &plan.src, !plan.same_meta));
        st.reextracted = plan.fresh.len() as u32;
        st.added = plan.old_of.iter().filter(|&&f| f == u32::MAX).count() as u32;
        st.removed = plan.new_of.iter().filter(|&&f| f == u32::MAX).count() as u32;
        let t1 = Instant::now();
        st.plan = t1 - t0;

        let fresh = timing::time("incremental.extract", || index::load(plan.fresh));
        st.bytes = fresh.stats().bytes;
        let t2 = Instant::now();
        st.extract = t2 - t1;

        let old = std::mem::replace(&mut self.index, index::load(Inputs::new()));
        self.index = old.patch(inputs, &plan.src, fresh);
        let mut compacted = false;
        let n = self.index.strings().len();
        if n >= MIN_COMPACT_STRINGS && n >= 2 * self.strings_base {
            let (live, dead) = timing::time("incremental.compact", || self.index.compact_strings());
            self.strings_base = live;
            compacted = dead > 0;
            st.strings_dropped = dead as u32;
        }
        let t3 = Instant::now();
        st.patch = t3 - t2;

        let imports = resolve_imports(&self.index, &self.attr);
        let t4 = Instant::now();
        st.resolve_imports = t4 - t3;
        let calls = self.calls.is_some().then(|| resolve_calls(&self.index, &imports));
        let t5 = Instant::now();
        st.resolve_calls = t5 - t4;

        let mut d = timing::time("incremental.diff", || {
            let mut d = snap.diff_nodes(&self.index, &plan.src, &plan.old_of);
            let none = |_| false;
            let (map, removed, fnodes) = (&d.node_map, &d.removed_files, snap.file_nodes());
            let mut ie = Vec::new();
            edges::import_deltas(&self.index, map, &self.imports, &imports, &plan.old_of, removed, fnodes, none, &mut ie);
            if let (Some(old), Some(new)) = (&self.calls, &calls) {
                let (mut c, mut o) = (Vec::new(), Vec::new());
                edges::call_deltas(&self.index, map, old, new, &plan.old_of, removed, none, &mut c, &mut o);
                d.call_edges = c;
                d.override_edges = o;
            }
            d.import_edges = ie;
            d
        });
        d.strings_compacted = compacted;
        debug_assert!(d.node_map.iter().all(|&v| v == NONE || v < self.index.node_count()));
        self.imports = imports;
        self.calls = calls;
        let t6 = Instant::now();
        st.diff = t6 - t5;
        st.total = t6 - t0;
        (d, st)
    }
}
