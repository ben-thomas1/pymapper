//! Incremental updates (design: Semantics, Incremental API): changed files -> re-extract ->
//! patch the index -> re-resolve dependants -> index diff.
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
//!     └─> snapshot     the little the diff needs from the old index (nodes of dropped files,
//!     │                run starts to translate rows)
//!     └─> extract      only the re-extracted files, with the normal parallel loader
//!     └─> patch        fresh strings merged into the interner (old Syms stay valid); facts and
//!     │                metrics spliced in place (one memmove pass per column); hierarchy and
//!     │                lookup tables rebuilt
//!     └─> compaction   the interner keeps strings of removed rows as tombstones; once it has
//!     │                doubled since the last compaction, live strings are marked and the
//!     │                interner is rebuilt (amortised O(1) per added string)
//!     └─> nodes        old node -> new node (file identity, def path, qualified id)
//!     └─> resolve      only the files that read a changed file (recorded dependencies: the
//!     │                reverse import graph made exact, see [`crate::resolve::incr`]); the
//!     │                results of every other file are carried over with translated ids
//!     └─> diff         added / removed / changed / moved nodes, import, call and override
//!                      edge deltas (see [`IndexDiff`])
//! ```
//!
//! When files are added, removed, renamed or reordered, or module names or statuses change
//! (a new stub, a new `__init__.py` elsewhere), the module set may change under every import:
//! the update then resolves the whole program again (still re-extracting only what changed).
//!
//! # Equivalence
//!
//! The patched index has the same files, rows, nodes and qualified ids as a fresh load of the
//! same inputs (only `Sym` values differ, which never matters), and the carried resolution
//! results are those a full resolution computes, because nothing they read changed. The
//! property tests check the whole state against a full rebuild after random edits (changes,
//! additions, removals, stubs, renames) on corpora.
//!
//! # Cost
//!
//! Reading and extracting, the dominant cost of a load, is paid only for re-extracted files,
//! and per-file resolution only for affected files. The rest is linear in the size of the
//! index with small constants: the splice moves the rows behind the first changed file once
//! (memmove), the hierarchy and the global resolution tables (module trie, namespaces,
//! classes, MROs, method index) are rebuilt, carried results are copied with translated ids,
//! and the diff translates old nodes by offset. Dependency tracking costs 4 bytes per
//! (file, file read) pair; see [`Program::tracking_bytes`].

mod diff;
mod edges;
mod plan;

#[cfg(test)]
mod tests;

pub use diff::{EdgeDelta, EdgeLabel, IndexDiff};

use std::path::Path;
use std::time::{Duration, Instant};

use crate::index::{self, Index, Inputs, NONE, Src};
use crate::intern::{FxHashSet, Sym};
use crate::resolve::incr::{self, CallSplit, Carry, Deps};
use crate::resolve::{Attribution, Imports, ResolvedCalls};
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
    /// Files whose imports / calls were resolved again (the others were carried over).
    pub reresolved_imports: u32,
    pub reresolved_calls: u32,
    /// The whole program was resolved again (files added, removed, renamed or reordered,
    /// module names or statuses changed, the interner was compacted, or a name the resolvers
    /// look up by text was interned for the first time).
    pub full_resolve: bool,
    pub plan: Duration,
    pub extract: Duration,
    pub patch: Duration,
    pub resolve_imports: Duration,
    pub resolve_calls: Duration,
    pub diff: Duration,
    pub total: Duration,
}

/// Call resolution with its tracking state.
struct Calls {
    rc: ResolvedCalls,
    deps: Deps,
    split: CallSplit,
}

/// A loaded program kept up to date by [`Program::update`] (see the module docs).
pub struct Program {
    index: Index,
    imports: Imports,
    import_deps: Deps,
    calls: Option<Calls>,
    attr: Attribution,
    /// Interner size after the last compaction (or the load).
    strings_base: usize,
    /// [`incr::names_key`] of the index.
    names_key: Vec<Option<Sym>>,
}

impl Program {
    /// Loads `inputs`, resolves imports against `attr` and, with `calls`, call sites.
    pub fn load(inputs: Inputs, attr: Attribution, calls: bool) -> Program {
        let index = index::load(inputs);
        Program::from_parts(index, attr, calls)
    }

    /// A program from an already loaded index (resolves it, recording dependencies).
    pub fn from_parts(index: Index, attr: Attribution, calls: bool) -> Program {
        let (imports, import_deps) = incr::resolve_imports_tracked(&index, &attr);
        let calls = calls.then(|| {
            let (rc, deps, split) = incr::resolve_calls_tracked(&index, &imports);
            Calls { rc, deps, split }
        });
        let strings_base = index.strings().len();
        let names_key = incr::names_key(&index);
        Program { index, imports, import_deps, calls, attr, strings_base, names_key }
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
        self.calls.as_ref().map(|c| &c.rc)
    }

    #[inline]
    pub fn attribution(&self) -> &Attribution {
        &self.attr
    }

    pub fn into_parts(self) -> (Index, Imports, Option<ResolvedCalls>) {
        (self.index, self.imports, self.calls.map(|c| c.rc))
    }

    /// Heap bytes of the index, the resolution tables and the dependency tracking.
    pub fn heap_bytes(&self) -> usize {
        self.index.heap_bytes() + self.imports.heap_bytes() + self.attr.heap_bytes() + self.calls.as_ref().map_or(0, |c| c.rc.heap_bytes()) + self.tracking_bytes()
    }

    /// Heap bytes of the dependency tracking alone (included in [`Program::heap_bytes`]).
    pub fn tracking_bytes(&self) -> usize {
        self.import_deps.heap_bytes() + self.calls.as_ref().map_or(0, |c| c.deps.heap_bytes() + c.split.capacity() * 4)
    }

    /// Recorded dependencies: (file, file read) pairs of the import and call resolutions.
    pub fn dependency_entries(&self) -> (usize, usize) {
        (self.import_deps.entries(), self.calls.as_ref().map_or(0, |c| c.deps.entries()))
    }

    /// Brings the program to the state of a full load of `inputs`, re-extracting only the
    /// files listed in `changed` (paths as in the inputs), the files not loaded before and
    /// files whose order changed. Files of the old inputs missing from `inputs` are removed.
    /// Files whose contents changed but are not listed keep their old facts.
    ///
    /// Returns what changed (see [`IndexDiff`]). Phases are recorded with [`timing`] as
    /// `incremental.*` and `resolve.*`.
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
        self.index = old.patch(inputs, &plan.src, fresh, plan.same_meta);
        let mut compacted = false;
        let n = self.index.strings().len();
        if n >= MIN_COMPACT_STRINGS && n >= 2 * self.strings_base {
            let (live, dead) = timing::time("incremental.compact", || self.index.compact_strings());
            self.strings_base = live;
            compacted = dead > 0;
            st.strings_dropped = dead as u32;
        }
        let mut d = timing::time("incremental.nodes", || snap.diff_nodes(&self.index, &plan.src, &plan.old_of));
        d.strings_compacted = compacted;
        let t3 = Instant::now();
        st.patch = t3 - t2;

        let key = incr::names_key(&self.index);
        let carry_ok = plan.same_meta && !compacted && key == self.names_key;
        self.names_key = key;
        st.full_resolve = !carry_ok;
        let ix = &self.index;
        let nf = ix.file_count() as usize;
        let carry = Carry { map: &d.node_map, imports: &snap.imports, calls: &snap.calls, decorators: &snap.decorators };
        let dirty: Vec<bool> = plan.src.iter().map(|s| matches!(s, Src::Fresh(_))).collect();
        let (imports, aff_imp) = if carry_ok {
            incr::reresolve_imports(ix, &self.attr, &self.imports, &mut self.import_deps, &dirty, &carry)
        } else {
            let (imp, deps) = incr::resolve_imports_tracked(ix, &self.attr);
            self.import_deps = deps;
            (imp, vec![true; nf])
        };
        st.reresolved_imports = aff_imp.iter().filter(|&&a| a).count() as u32;
        let t4 = Instant::now();
        st.resolve_imports = t4 - t3;

        let mut aff_calls = vec![true; nf];
        let calls = match self.calls.take() {
            Some(c) if carry_ok => {
                // Calls read the import results of other files: affected files whose results
                // changed are dirty too.
                let mut dirty_calls = dirty.clone();
                for f in 0..nf {
                    if aff_imp[f] && !dirty_calls[f] {
                        dirty_calls[f] = incr::import_results_changed(&self.imports, &imports, &carry, ix, f as u32);
                    }
                }
                let mut touched: FxHashSet<Sym> = snap.dropped_names().collect();
                for (i, s) in plan.src.iter().enumerate() {
                    if matches!(s, Src::Fresh(_)) {
                        let fnode = ix.file_node(i as u32);
                        for dd in 1..ix.file_facts(i as u32).defs.len {
                            touched.insert(ix.name_sym(fnode + dd));
                        }
                    }
                }
                let Calls { rc, mut deps, mut split } = c;
                let (new, aff) = incr::reresolve_calls(ix, &imports, &rc, &mut deps, &mut split, &dirty_calls, &carry, &touched);
                aff_calls = aff;
                Some((rc, Calls { rc: new, deps, split }))
            }
            Some(c) => {
                let (new, deps, split) = incr::resolve_calls_tracked(ix, &imports);
                Some((c.rc, Calls { rc: new, deps, split }))
            }
            None => None,
        };
        st.reresolved_calls = if calls.is_some() { aff_calls.iter().filter(|&&a| a).count() as u32 } else { 0 };
        let t5 = Instant::now();
        st.resolve_calls = t5 - t4;

        timing::time("incremental.diff", || {
            let (map, removed, fnodes) = (&d.node_map, &d.removed_files, snap.file_nodes());
            let mut ie = Vec::new();
            let skip_i = |f: u32| carry_ok && !aff_imp[f as usize];
            edges::import_deltas(ix, map, &self.imports, &imports, &plan.old_of, removed, fnodes, skip_i, &mut ie);
            if let Some((old_rc, c)) = &calls {
                let (mut ce, mut oe) = (Vec::new(), Vec::new());
                let skip_c = |f: u32| carry_ok && !aff_calls[f as usize];
                edges::call_deltas(ix, map, old_rc, &c.rc, &plan.old_of, removed, skip_c, &mut ce, &mut oe);
                d.call_edges = ce;
                d.override_edges = oe;
            }
            d.import_edges = ie;
        });
        debug_assert!(d.node_map.iter().all(|&v| v == NONE || v < self.index.node_count()));
        self.imports = imports;
        self.calls = calls.map(|(_, c)| c);
        let t6 = Instant::now();
        st.diff = t6 - t5;
        st.total = t6 - t0;
        (d, st)
    }
}
