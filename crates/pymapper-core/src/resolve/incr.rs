//! Incremental re-resolution (see [`crate::incremental`]): resolve only the files an update
//! can affect and carry the results of every other file over.
//!
//! # Dependencies
//!
//! A tracked resolution records, per file, every file whose facts or results its lookups read
//! ([`Deps`]): the files of every module a name lookup visits (implementation and stub,
//! through re-exports, star imports and aliases), and for calls also the files of the scopes,
//! assignments, class members and MRO entries the resolver reads. Resolution is a function of
//! those reads, so a file needs re-resolving only when one of them changed:
//!
//! ```text
//!   dirty imports  = re-extracted files
//!   affected imp.  = dirty ∪ { f : deps_imports(f) ∩ dirty ≠ ∅ }
//!   dirty calls    = dirty ∪ affected files whose import results changed
//!                  ∪ files of classes whose MRO or flags changed (MROs are rebuilt globally)
//!   affected calls = dirty calls ∪ { f : deps_calls(f) ∩ dirty calls ≠ ∅ }
//!                  ∪ files with a call named like a method whose candidates changed
//!                    (unique-method and subclass fallbacks look methods up by name)
//! ```
//!
//! This is the reverse import graph made exact: a file depends on what its lookups actually
//! visited, not on every module it imports, and transitively through re-export chains (the
//! visits of a lookup include every hop). A carried result that would point at a node that is
//! gone makes its file affected instead (a safety net; the property tests check that it never
//! changes the outcome).
//!
//! Results of files that are not affected are translated in place (node ids by offset, rows
//! by their file's shift) and the affected files' rows are spliced in, so the old and new
//! tables never coexist. The module trie only moves its nodes and namespaces only shift rows;
//! the class table, MROs and method index are rebuilt (see `docs/benchmarks.md`).
//!
//! Preconditions of the incremental entry points: the old and new index have the same files
//! in the same order with the same module names and statuses (only contents changed), and the
//! interner was not compacted. Otherwise callers run a full tracked resolution.

use std::time::Instant;

use rayon::prelude::*;

use super::calls::{CallChunk, FileScratch, Names as CallNames, Tables, class_bases, file_calls};
use super::classes::{Classes, MethodIndex};
use super::imports::{Ctx, ModRes, NameRes, Names, Scratch};
use super::lookup::Cx;
use super::modules::ModuleTable;
use super::namespace::{Namespaces, chunk_size};
use super::{
    Attribution, CallEdge, CallStats, ExtEdge, ImportStats, Imports, ModuleEdge, ResolvedCalls, edges,
};
use crate::extract::splice::{Splice, splice_col};
use crate::index::{FileId, Index, NONE, NodeId, NodeShift};
use crate::intern::{FxHashSet, Sym};
use crate::timing;

/// Files each file's resolution read, per file sorted and without the file itself (CSR).
#[derive(Debug, Default)]
pub(crate) struct Deps {
    start: Vec<u32>,
    files: Vec<FileId>,
}

impl Deps {
    /// Files read by file `f`'s resolution.
    #[inline]
    pub fn of(&self, f: FileId) -> &[FileId] {
        &self.files[self.start[f as usize] as usize..self.start[f as usize + 1] as usize]
    }

    /// Number of files.
    pub fn len(&self) -> usize {
        self.start.len().saturating_sub(1)
    }

    /// Total dependency entries.
    pub fn entries(&self) -> usize {
        self.files.len()
    }

    pub fn heap_bytes(&self) -> usize {
        (self.start.capacity() + self.files.capacity()) * 4
    }

    /// Marks in `out` every file that read a file marked in `dirty` (and the dirty ones).
    pub fn readers(&self, dirty: &[bool], out: &mut [bool]) {
        for f in 0..self.len() {
            if dirty[f] || self.of(f as FileId).iter().any(|&g| dirty[g as usize]) {
                out[f] = true;
            }
        }
    }

    /// Replaces the lists of the files in `fresh` (ascending, with their new lists in order).
    fn replace(&mut self, fresh: &[FileId], lists: &[FileId], lens: &[u32]) {
        let n = self.len();
        let mut start = Vec::with_capacity(n + 1);
        let mut files = Vec::with_capacity(self.files.len() + lists.len());
        start.push(0u32);
        let (mut k, mut at) = (0usize, 0usize);
        for f in 0..n as FileId {
            if k < fresh.len() && fresh[k] == f {
                let l = lens[k] as usize;
                files.extend_from_slice(&lists[at..at + l]);
                at += l;
                k += 1;
            } else {
                files.extend_from_slice(self.of(f));
            }
            start.push(files.len() as u32);
        }
        files.shrink_to_fit();
        *self = Deps { start, files };
    }
}

/// Sorts and dedups a file's log, dropping the file itself, and appends it to `out`.
fn push_log(log: &mut Vec<FileId>, f: FileId, out: &mut Vec<FileId>) -> u32 {
    log.sort_unstable();
    log.dedup();
    let before = out.len();
    out.extend(log.iter().copied().filter(|&g| g != f));
    log.clear();
    (out.len() - before) as u32
}

/// Lists of at most this many files are resolved on the calling thread (waking the pool
/// costs more than resolving them).
const INLINE_FILES: usize = 4;

/// Per-chunk output of a tracked pass over a list of files.
#[derive(Default)]
struct Part<T> {
    rows: Vec<T>,
    lens: Vec<u32>,
    deps: Vec<FileId>,
    dep_lens: Vec<u32>,
}

/// Runs `per(state, file, rows, log)` over `files` (ascending) in parallel chunks; returns the
/// rows in order with per-file lengths, and the per-file dependency lists.
fn par_files<T: Send, S: Default>(
    files: &[FileId],
    per: impl Fn(&mut S, FileId, &mut Vec<T>, &mut Vec<FileId>) + Sync,
) -> (Vec<T>, Vec<u32>, Vec<FileId>, Vec<u32>) {
    let chunk = chunk_size(files.len());
    let run = |fs: &[FileId]| {
        let mut st = S::default();
        let mut p = Part { rows: Vec::new(), lens: Vec::with_capacity(fs.len()), deps: Vec::new(), dep_lens: Vec::new() };
        let mut log = Vec::new();
        for &f in fs {
            let before = p.rows.len();
            per(&mut st, f, &mut p.rows, &mut log);
            p.lens.push((p.rows.len() - before) as u32);
            p.dep_lens.push(push_log(&mut log, f, &mut p.deps));
        }
        p
    };
    // A few files (the usual update): on this thread, without waking the pool.
    let parts: Vec<Part<T>> =
        if files.len() <= INLINE_FILES { vec![run(files)] } else { files.par_chunks(chunk.max(1)).map(run).collect() };
    let (mut rows, mut lens, mut deps, mut dep_lens) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    rows.reserve_exact(parts.iter().map(|p| p.rows.len()).sum());
    deps.reserve_exact(parts.iter().map(|p| p.deps.len()).sum());
    for mut p in parts {
        rows.append(&mut p.rows);
        lens.append(&mut p.lens);
        deps.append(&mut p.deps);
        dep_lens.append(&mut p.dep_lens);
    }
    (rows, lens, deps, dep_lens)
}

/// How results of the old index translate to the new one (same files, same order).
pub(crate) struct Carry<'a> {
    /// New node of every old node (`NONE`: gone).
    pub map: &'a [NodeId],
    /// The same by offset, with the defs of re-extracted files gone (when the hierarchy was
    /// patched): carried results translate through it (no random access into `map`).
    pub shift: Option<&'a NodeShift>,
    /// Some old node is gone (a def removed or renamed in a re-extracted file): carried
    /// results are checked for references to it.
    pub dead: bool,
    /// Old run starts per file (`files + 1` entries, the last is the table length) of the
    /// import, call, decorator and binding tables.
    pub imports: &'a [u32],
    pub calls: &'a [u32],
    pub decorators: &'a [u32],
    pub bindings: &'a [u32],
}

impl Carry<'_> {
    /// New id of old node `v` (`NONE`: gone): by offset where possible, else through the
    /// node map (defs of re-extracted files matched by def path).
    #[inline]
    fn node(&self, v: NodeId) -> NodeId {
        if v == NONE {
            return NONE;
        }
        match self.shift.map(|s| s.translate(v)) {
            Some(w) if w != NONE => w,
            _ => self.map.get(v as usize).copied().unwrap_or(NONE),
        }
    }
}

/// New row of old row `row` of a table with old run starts `old` and new run starts given
/// by `new_start(file)`. `NONE` stays `NONE`.
#[inline]
fn shift_row(row: u32, old: &[u32], new_start: impl Fn(FileId) -> u32) -> u32 {
    if row == NONE {
        return NONE;
    }
    // The file whose run holds the row (runs may be empty: the last run starting at or
    // before the row that is non-empty).
    let f = old.partition_point(|&s| s <= row) - 1;
    row - old[f] + new_start(f as FileId)
}

/// Files of `affected` (ascending).
fn listed(affected: &[bool]) -> Vec<FileId> {
    (0..affected.len() as FileId).filter(|&f| affected[f as usize]).collect()
}

/// Runs `per(file, run)` in parallel over the runs `start[f]..start[f + 1]` of `v` of every
/// file not in `skip` (disjoint mutable slices: no copy, no lock).
pub(super) fn par_runs_mut<T: Send>(v: &mut [T], start: &[u32], skip: &[bool], per: impl Fn(usize, &mut [T]) + Sync) {
    let mut runs: Vec<(usize, &mut [T])> = Vec::with_capacity(skip.len());
    let mut rest = v;
    for f in 0..skip.len() {
        let len = (start[f + 1] - start[f]) as usize;
        let (run, tail) = rest.split_at_mut(len);
        rest = tail;
        if !skip[f] && len > 0 {
            runs.push((f, run));
        }
    }
    runs.into_par_iter().with_min_len(64).for_each(|(f, run)| per(f, run));
}

/// Old rows of the files of `files` (ascending): `start` runs per file, copied out of `v`.
fn save<T: Copy>(v: &[T], start: &[u32], files: &[FileId]) -> Saved<T> {
    let mut s = Saved { files: files.to_vec(), start: Vec::with_capacity(files.len() + 1), rows: Vec::new() };
    s.start.push(0);
    for &f in files {
        s.rows.extend_from_slice(&v[start[f as usize] as usize..start[f as usize + 1] as usize]);
        s.start.push(s.rows.len() as u32);
    }
    s
}

/// Rows of some files saved before an in-place update (old ids).
#[derive(Debug, Default)]
pub(crate) struct Saved<T> {
    files: Vec<FileId>,
    start: Vec<u32>,
    rows: Vec<T>,
}

impl<T> Saved<T> {
    /// Saved rows of file `f` (empty if not saved).
    pub fn get(&self, f: FileId) -> &[T] {
        match self.files.binary_search(&f) {
            Ok(k) => &self.rows[self.start[k] as usize..self.start[k + 1] as usize],
            Err(_) => &[],
        }
    }

    fn at(&self, k: usize) -> &[T] {
        &self.rows[self.start[k] as usize..self.start[k + 1] as usize]
    }
}

/// Splices the per-file runs of the `files` (ascending) into `v`: the old runs (`old_start`)
/// become the fresh rows (`fresh`, consecutive per file with lengths `lens`).
pub(super) fn splice_files<T: Copy>(v: &mut Vec<T>, old_start: &[u32], files: &[FileId], fresh: &[T], lens: &[u32]) {
    let mut plan = Vec::with_capacity(files.len());
    let mut at = 0u32;
    for (k, &f) in files.iter().enumerate() {
        let (s, e) = (old_start[f as usize], old_start[f as usize + 1]);
        plan.push(Splice { at: s, old: e - s, src: at, new: lens[k] });
        at += lens[k];
    }
    splice_col(v, &plan, fresh);
}

/// Per-file run starts after replacing the runs of `files` (lengths `lens`).
pub(super) fn new_starts(old_start: &[u32], files: &[FileId], lens: &[u32]) -> Vec<u32> {
    let n = old_start.len() - 1;
    let mut s = Vec::with_capacity(n + 1);
    s.push(0u32);
    let (mut k, mut at) = (0usize, 0u32);
    for f in 0..n {
        if k < files.len() && files[k] as usize == f {
            at += lens[k];
            k += 1;
        } else {
            at += old_start[f + 1] - old_start[f];
        }
        s.push(at);
    }
    s
}

/// The global tables of the import pass, timed as `resolve.modules`.
fn import_tables(ix: &Index) -> (ModuleTable, Namespaces) {
    timing::time("resolve.modules", || rayon::join(|| ModuleTable::build(ix), || Namespaces::build(ix)))
}

/// Phase 1 of `files`: rows in order and per-file lengths.
fn p1_of(cx: &Ctx<'_>, attr: &Attribution, files: &[FileId]) -> (Vec<ModRes>, Vec<u32>) {
    let (rows, lens, _, _) = par_files(files, |_: &mut (), f, out: &mut Vec<ModRes>, _| {
        for row in cx.ix.file_facts(f).imports.range() {
            out.push(cx.module_part(attr, f, row));
        }
    });
    (rows, lens)
}

/// Phase 2 of `files` (logged): rows, per-file lengths, dependency lists and their lengths.
fn p2_of(names: &Names<'_>, files: &[FileId]) -> (Vec<NameRes>, Vec<u32>, Vec<FileId>, Vec<u32>) {
    let ix = names.cx.ix;
    par_files(files, |sc: &mut Scratch, f, out: &mut Vec<NameRes>, log| {
        sc.logging = true;
        for row in ix.file_facts(f).imports.range() {
            out.push(names.row(f, row, sc));
        }
        log.append(&mut sc.log);
    })
}

/// Import statistics of a finished resolution.
fn import_stats(imp: &mut Imports, t: [Instant; 4]) {
    let mut stats = ImportStats {
        rows: imp.p2.len() as u64,
        module_edges: imp.edges.len() as u64,
        ext_edges: imp.ext_edges.len() as u64,
        modules: u64::from(imp.modules.len()),
        build: t[1] - t[0],
        resolve: t[2] - t[1],
        edges: t[3] - t[2],
        ..ImportStats::default()
    };
    for r in &imp.p2 {
        stats.by_status[r.status as usize] += 1;
    }
    imp.stats = stats;
}

/// [`super::resolve_imports`] that also records every file's dependencies.
pub(crate) fn resolve_imports_tracked(ix: &Index, attr: &Attribution) -> (Imports, Deps) {
    let t0 = Instant::now();
    let (modules, ns) = import_tables(ix);
    let t1 = Instant::now();
    let getattr = ix.strings().get(super::builtins::texts::GETATTR);
    let cx = Ctx { ix, mods: &modules, ns: &ns, getattr };
    let all: Vec<FileId> = (0..ix.file_count()).collect();
    let (p1, p2, files, lens) = timing::time("resolve.imports", || {
        let (p1, _) = p1_of(&cx, attr, &all);
        let (p2, _, files, lens) = p2_of(&Names { cx: &cx, p1: &p1 }, &all);
        (p1, p2, files, lens)
    });
    let t2 = Instant::now();
    let ((edges, el), (ext_edges, xl)) = timing::time("resolve.edges", || {
        let (e, el, _, _) = par_files(&all, |_: &mut (), f, out: &mut Vec<ModuleEdge>, _| edges::file_edges(ix, &modules, &p1, &p2, f, out));
        let (x, xl, _, _) = par_files(&all, |_: &mut (), f, out: &mut Vec<ExtEdge>, _| edges::file_ext_edges(ix, &p1, &p2, f, out));
        ((e, el), (x, xl))
    });
    let t3 = Instant::now();
    let mut imp = Imports {
        modules,
        ns,
        p1,
        p2,
        edges,
        edge_start: starts(&el),
        ext_edges,
        ext_start: starts(&xl),
        getattr,
        stats: ImportStats::default(),
    };
    import_stats(&mut imp, [t0, t1, t2, t3]);
    (imp, Deps { start: starts(&lens), files })
}

/// What [`reresolve_imports`] did besides the new resolution.
pub(crate) struct ImportsUpdate {
    /// Files resolved again.
    pub affected: Vec<bool>,
    /// Affected files whose results differ from their old ones (re-extracted files count as
    /// changed).
    pub changed: Vec<bool>,
    /// Old module edges of the affected files (old ids), for the diff.
    pub old_edges: Saved<ModuleEdge>,
}

/// Re-resolves the imports of `ix` after an update (see the module docs), updating `old` in
/// place: carried rows are translated where they are, affected files' rows are spliced in.
/// `dirty` marks the re-extracted files; `deps` is updated.
pub(crate) fn reresolve_imports(
    ix: &Index,
    attr: &Attribution,
    old: Imports,
    deps: &mut Deps,
    dirty: &[bool],
    carry: &Carry<'_>,
) -> (Imports, ImportsUpdate) {
    let n = ix.file_count() as usize;
    let g = timing::phase("incremental.affected");
    let mut affected = vec![false; n];
    deps.readers(dirty, &mut affected);
    // Safety net: a carried result must not point at a node that is gone, nor at a re-export
    // row of a re-extracted file (the dependencies already cover both: the lookup that found
    // them read their file). Re-export rows are rare, so the row check costs one compare per
    // row; nodes are only checked when some node is gone.
    affected.par_iter_mut().enumerate().with_min_len(256).for_each(|(f, a)| {
        if !*a {
            let rows = &old.p2[carry.imports[f] as usize..carry.imports[f + 1] as usize];
            *a = rows.iter().any(|r| {
                (r.via != NONE && dirty[carry.imports.partition_point(|&s| s <= r.via) - 1])
                    || (carry.dead && r.target != NONE && carry.node(r.target) == NONE)
            });
        }
    });
    drop(g);
    let fresh = listed(&affected);
    let fresh_dirty = listed(dirty);
    let Imports { modules, ns, mut p1, mut p2, mut edges, edge_start, mut ext_edges, ext_start, getattr, stats: old_stats } = old;
    let t0 = Instant::now();
    // Same files, names and statuses: the module trie only moves its nodes, namespaces only
    // shift their rows outside the re-extracted files.
    let (modules, ns) = timing::time("resolve.modules", || {
        let imports_start = |f: usize| ix.file_facts(f as FileId).imports.start;
        let bindings_start = |f: usize| ix.file_facts(f as FileId).bindings.start;
        rayon::join(
            || modules.remap_nodes(|v| carry.node(v)),
            || {
                ns.patch(ix, &fresh_dirty, dirty, |f| {
                    (
                        i64::from(imports_start(f)) - i64::from(carry.imports[f]),
                        i64::from(bindings_start(f)) - i64::from(carry.bindings[f]),
                    )
                })
            },
        )
    });
    let t1 = Instant::now();
    let cx = Ctx { ix, mods: &modules, ns: &ns, getattr };
    let new_start = |f: FileId| ix.file_facts(f).imports.start;
    let saved_p1 = save(&p1, carry.imports, &fresh);
    let saved_p2 = save(&p2, carry.imports, &fresh);
    let saved_x = save(&ext_edges, &ext_start, &fresh);
    let old_edges = save(&edges, &edge_start, &fresh);
    let (files, lens) = timing::time("resolve.imports", || {
        let (p1_fresh, p1_lens) = p1_of(&cx, attr, &fresh);
        splice_files(&mut p1, carry.imports, &fresh, &p1_fresh, &p1_lens);
        drop(p1_fresh);
        par_runs_mut(&mut p2, carry.imports, &affected, |_, run| {
            for r in run {
                r.target = carry.node(r.target);
                r.via = shift_row(r.via, carry.imports, new_start);
            }
        });
        let (p2_fresh, p2_lens, files, lens) = p2_of(&Names { cx: &cx, p1: &p1 }, &fresh);
        splice_files(&mut p2, carry.imports, &fresh, &p2_fresh, &p2_lens);
        (files, lens)
    });
    debug_assert_eq!(p2.len(), ix.facts().imports.len() as usize);
    let t2 = Instant::now();
    let (edge_start, ext_start) = timing::time("resolve.edges", || {
        par_runs_mut(&mut edges, &edge_start, &affected, |_, run| {
            for e in run {
                e.dst = carry.node(e.dst);
            }
        });
        let (e, el, _, _) = par_files(&fresh, |_: &mut (), f, out: &mut Vec<ModuleEdge>, _| edges::file_edges(ix, &modules, &p1, &p2, f, out));
        let (x, xl, _, _) = par_files(&fresh, |_: &mut (), f, out: &mut Vec<ExtEdge>, _| edges::file_ext_edges(ix, &p1, &p2, f, out));
        splice_files(&mut edges, &edge_start, &fresh, &e, &el);
        splice_files(&mut ext_edges, &ext_start, &fresh, &x, &xl);
        (new_starts(&edge_start, &fresh, &el), new_starts(&ext_start, &fresh, &xl))
    });
    let t3 = Instant::now();
    let mut imp = Imports { modules, ns, p1, p2, edges, edge_start, ext_edges, ext_start, getattr, stats: old_stats };
    // Statistics: the old counts minus the old rows of the re-resolved files plus their new
    // rows (no pass over every row).
    let st = &mut imp.stats;
    for r in &saved_p2.rows {
        st.by_status[r.status as usize] -= 1;
    }
    for &f in &fresh {
        for r in &imp.p2[ix.file_facts(f).imports.range()] {
            st.by_status[r.status as usize] += 1;
        }
    }
    st.rows = imp.p2.len() as u64;
    st.module_edges = imp.edges.len() as u64;
    st.ext_edges = imp.ext_edges.len() as u64;
    st.modules = u64::from(imp.modules.len());
    (st.build, st.resolve, st.edges) = (t1 - t0, t2 - t1, t3 - t2);
    deps.replace(&fresh, &files, &lens);
    // Which affected files' results changed (translated old rows against the new ones).
    let mut changed = dirty.to_vec();
    for (k, &f) in fresh.iter().enumerate() {
        if dirty[f as usize] {
            continue;
        }
        let r = ix.file_facts(f).imports.range();
        let (o1, o2) = (saved_p1.at(k), saved_p2.at(k));
        let same = o1.len() == r.len()
            && o1.iter().zip(&imp.p1[r.clone()]).all(|(a, b)| {
                a.module == b.module && a.ext == b.ext && a.status == b.status && a.matched == b.matched
            })
            && o2.iter().zip(&imp.p2[r]).all(|(a, b)| {
                carry.node(a.target) == b.target
                    && shift_row(a.via, carry.imports, new_start) == b.via
                    && a.status == b.status
                    && a.flags == b.flags
            })
            && {
                let (oe, ne) = (old_edges.at(k), imp.module_edges(f));
                oe.len() == ne.len() && oe.iter().zip(ne).all(|(a, b)| carry.node(a.dst) == b.dst && a.rows == b.rows && a.flags == b.flags)
            }
            && saved_x.at(k) == imp.ext_edges(f);
        changed[f as usize] = !same;
    }
    (imp, ImportsUpdate { affected, changed, old_edges })
}

/// The global tables of the call pass: interned names, builtins, classes with their MROs and
/// the method index (rebuilt by every resolution).
struct CallTables {
    names: CallNames,
    tables: Tables,
    classes: Classes,
    methods: MethodIndex,
    t_classes: std::time::Duration,
    t_mro: std::time::Duration,
}

fn call_tables(ix: &Index, imp: &Imports) -> CallTables {
    let names = CallNames::new(ix);
    let tables = Tables::new(ix);
    let t0 = Instant::now();
    let (mut classes, methods) = timing::time("resolve.classes", || {
        rayon::join(|| Classes::build(ix, names.self_, names.getattr), || MethodIndex::build(ix))
    });
    let t1 = Instant::now();
    timing::time("resolve.mro", || {
        let (bases, base_start, flags) = {
            let cx = Cx { ix, imp, classes: &classes, tables: &tables, names };
            class_bases(&cx)
        };
        classes.set_bases(bases, base_start, &flags);
        classes.linearize();
    });
    CallTables { names, tables, classes, methods, t_classes: t1 - t0, t_mro: t1.elapsed() }
}

/// Per-chunk output of the tracked call pass.
#[derive(Default)]
struct CallPart {
    out: CallChunk,
    deps: Vec<FileId>,
    dep_lens: Vec<u32>,
}

/// The tracking state of a call resolution besides its dependencies: per file, how many of
/// its edges come from call rows (the others: decorators, then overrides).
pub(crate) type CallSplit = Vec<u32>;

/// Fresh call results of `files` (logged), concatenated: info rows, edges with per-file
/// edge counts and call-edge counts, dependency lists with lengths.
struct FreshCalls {
    info: Vec<u8>,
    edges: Vec<CallEdge>,
    edge_lens: Vec<u32>,
    call_lens: Vec<u32>,
    deps: Vec<FileId>,
    dep_lens: Vec<u32>,
}

fn calls_of(ix: &Index, imp: &Imports, t: &CallTables, files: &[FileId]) -> FreshCalls {
    let parts: Vec<CallPart> = timing::time("resolve.calls", || {
        let cx = Cx { ix, imp, classes: &t.classes, tables: &t.tables, names: t.names };
        let run = |fs: &[FileId]| {
            let mut sc = FileScratch::default();
            sc.loc.logging = true;
            let mut p = CallPart::default();
            let mut log = Vec::new();
            for &f in fs {
                file_calls(&cx, &t.methods, &mut sc, f, &mut p.out);
                log.append(sc.loc.log.get_mut());
                p.dep_lens.push(push_log(&mut log, f, &mut p.deps));
            }
            p
        };
        if files.len() <= INLINE_FILES {
            vec![run(files)]
        } else {
            files.par_chunks(chunk_size(files.len()).max(1)).map(run).collect()
        }
    });
    let mut out = FreshCalls {
        info: Vec::with_capacity(parts.iter().map(|p| p.out.info.len()).sum()),
        edges: Vec::with_capacity(parts.iter().map(|p| p.out.edges.len()).sum()),
        edge_lens: Vec::with_capacity(files.len()),
        call_lens: Vec::with_capacity(files.len()),
        deps: Vec::with_capacity(parts.iter().map(|p| p.deps.len()).sum()),
        dep_lens: Vec::with_capacity(files.len()),
    };
    for mut p in parts {
        out.info.append(&mut p.out.info);
        out.edges.append(&mut p.out.edges);
        out.edge_lens.append(&mut p.out.edge_lens);
        out.call_lens.append(&mut p.out.call_lens);
        out.deps.append(&mut p.deps);
        out.dep_lens.append(&mut p.dep_lens);
    }
    out
}

/// Call statistics of a finished resolution.
fn call_stats(rc: &mut ResolvedCalls, t: &CallTables, calls_time: std::time::Duration) {
    let mut stats = CallStats {
        calls: rc.info.len() as u64,
        edges: rc.edges.len() as u64,
        classes: u64::from(t.classes.len()),
        classes_time: t.t_classes,
        mro_time: t.t_mro,
        calls_time,
        ..CallStats::default()
    };
    for &v in &rc.info {
        stats.by_conf[(v >> 6) as usize] += 1;
        stats.by_reason[(v & 0x3f) as usize] += 1;
    }
    for e in &rc.edges {
        stats.edges_by_reason[e.reason as usize] += 1;
    }
    class_stats(&mut stats, t);
    rc.stats = stats;
}

/// Class counts and the class / MRO phase times of `t`.
fn class_stats(stats: &mut CallStats, t: &CallTables) {
    stats.classes = u64::from(t.classes.len());
    stats.classes_time = t.t_classes;
    stats.mro_time = t.t_mro;
    (stats.incomplete_classes, stats.no_c3) = (0, 0);
    for c in 0..t.classes.len() {
        let fl = t.classes.flags(c);
        stats.incomplete_classes += u64::from(fl & super::class_flags::INCOMPLETE != 0);
        stats.no_c3 += u64::from(fl & super::class_flags::NO_C3 != 0);
    }
}

/// [`super::resolve_calls`] that also records every file's dependencies and edge split.
pub(crate) fn resolve_calls_tracked(ix: &Index, imp: &Imports) -> (ResolvedCalls, Deps, CallSplit) {
    let t = call_tables(ix, imp);
    let all: Vec<FileId> = (0..ix.file_count()).collect();
    let t2 = Instant::now();
    let fc = calls_of(ix, imp, &t, &all);
    debug_assert_eq!(fc.info.len(), ix.facts().calls.len() as usize);
    let FreshCalls { info, edges, edge_lens, call_lens, deps, dep_lens } = fc;
    let mut rc = ResolvedCalls { classes: Classes::default(), methods: MethodIndex::default(), info, edges, edge_start: starts(&edge_lens), stats: CallStats::default() };
    let elapsed = t2.elapsed();
    call_stats(&mut rc, &t, elapsed);
    rc.classes = t.classes;
    rc.methods = t.methods;
    (rc, Deps { start: starts(&dep_lens), files: deps }, call_lens)
}

/// What [`reresolve_calls`] did besides the new resolution.
pub(crate) struct CallsUpdate {
    /// Files resolved again.
    pub affected: Vec<bool>,
    /// Old edges of the affected files (old ids), for the diff.
    pub old_edges: Saved<CallEdge>,
}

/// Re-resolves the calls of `ix` after an update (see the module docs), updating `old` in
/// place. `dirty` marks the re-extracted files and the files whose import results changed;
/// `touched` holds the def names of the re-extracted files, before and after. `deps` and
/// `split` are updated.
#[allow(clippy::too_many_arguments)]
pub(crate) fn reresolve_calls(
    ix: &Index,
    imp: &Imports,
    old: ResolvedCalls,
    deps: &mut Deps,
    split: &mut CallSplit,
    dirty: &[bool],
    carry: &Carry<'_>,
    touched: &FxHashSet<Sym>,
) -> (ResolvedCalls, CallsUpdate) {
    let t = call_tables(ix, imp);
    let n = ix.file_count() as usize;
    let g = timing::phase("incremental.affected");
    let mut dirty = dirty.to_vec();
    // Classes whose MRO or flags changed (as nodes): their files are dirty. A file that was
    // not re-extracted has the same classes in the same order in both tables.
    for (f, d) in dirty.iter_mut().enumerate() {
        if *d {
            continue;
        }
        let (or, nr) = (old.classes.file_classes(f as FileId), t.classes.file_classes(f as FileId));
        let same = or.len() == nr.len()
            && or.zip(nr).all(|(k, c)| {
                let (om, nm) = (old.classes.mro(k), t.classes.mro(c));
                old.classes.flags(k) == t.classes.flags(c)
                    && om.len() == nm.len()
                    && om.iter().zip(nm).all(|(&a, &b)| carry.node(old.classes.node(a)) == t.classes.node(b))
            });
        *d = !same;
    }
    let mut affected = vec![false; n];
    deps.readers(&dirty, &mut affected);
    // Method names whose candidates changed, where a lookup by name can see it (one candidate
    // for the unique-method fallback, at most the subclass limit for subclass methods).
    let mut names: FxHashSet<Sym> = FxHashSet::default();
    let (mut a, mut b) = (Vec::new(), Vec::new());
    for &s in touched {
        a.clear();
        b.clear();
        a.extend(old.methods.candidates(s).iter().map(|&(_, v)| carry.node(v)));
        b.extend(t.methods.candidates(s).iter().map(|&(_, v)| v));
        a.sort_unstable();
        b.sort_unstable();
        if a != b && a.len().min(b.len()) <= super::calls::MAX_SUBCLASS_CANDIDATES {
            names.insert(s);
        }
    }
    if !names.is_empty() {
        let hit = |c: &[Sym]| c.last().is_some_and(|s| names.contains(s)) || (c.len() >= 2 && names.contains(&c[1]));
        // Every call and decorator row of the program: parallel, like the safety net.
        affected.par_iter_mut().enumerate().with_min_len(256).for_each(|(f, a)| {
            if !*a {
                let fv = ix.file_view(f as FileId);
                let ff = ix.file_facts(f as FileId);
                *a = ff.calls.range().any(|row| hit(fv.call_chain(row)))
                    || ff.decorators.range().any(|row| hit(fv.decorator_chain(row)));
            }
        });
    }
    // Safety net: carried edges must not point at nodes that are gone.
    if carry.dead {
        affected.par_iter_mut().enumerate().with_min_len(256).for_each(|(f, a)| {
            if !*a {
                *a = old.file_edges(f as FileId).iter().any(|e| carry.node(e.src) == NONE || carry.node(e.dst) == NONE);
            }
        });
    }
    drop(g);
    let fresh = listed(&affected);
    let ResolvedCalls { mut info, mut edges, edge_start, stats: old_stats, .. } = old;
    let old_edges = save(&edges, &edge_start, &fresh);
    let old_info = save(&info, carry.calls, &fresh);
    let t2 = Instant::now();
    let fc = calls_of(ix, imp, &t, &fresh);
    let carry_phase = timing::phase("incremental.carry");
    let calls_start = |f: FileId| ix.file_facts(f).calls.start;
    let dec_start = |f: FileId| ix.file_facts(f).decorators.start;
    let osplit: &CallSplit = split;
    par_runs_mut(&mut edges, &edge_start, &affected, |f, run| {
        // Sites are rows of the file's own call / decorator runs: one shift per file.
        let k = osplit[f] as usize;
        let dc = i64::from(calls_start(f as FileId)) - i64::from(carry.calls[f]);
        let dd = i64::from(dec_start(f as FileId)) - i64::from(carry.decorators[f]);
        for (i, e) in run.iter_mut().enumerate() {
            e.src = carry.node(e.src);
            e.dst = carry.node(e.dst);
            if e.site != NONE {
                e.site = (i64::from(e.site) + if i < k { dc } else { dd }) as u32;
            }
        }
    });
    let info_lens: Vec<u32> = fresh.iter().map(|&f| ix.file_facts(f).calls.len).collect();
    splice_files(&mut info, carry.calls, &fresh, &fc.info, &info_lens);
    splice_files(&mut edges, &edge_start, &fresh, &fc.edges, &fc.edge_lens);
    let edge_start = new_starts(&edge_start, &fresh, &fc.edge_lens);
    for (k, &f) in fresh.iter().enumerate() {
        split[f as usize] = fc.call_lens[k];
    }
    drop(carry_phase);
    debug_assert_eq!(info.len(), ix.facts().calls.len() as usize);
    deps.replace(&fresh, &fc.deps, &fc.dep_lens);
    // Statistics: the old counts minus the old rows and edges of the re-resolved files plus
    // their new ones (no pass over every row).
    let mut st = old_stats;
    for &v in old_info.rows.iter() {
        st.by_conf[(v >> 6) as usize] -= 1;
        st.by_reason[(v & 0x3f) as usize] -= 1;
    }
    for &v in &fc.info {
        st.by_conf[(v >> 6) as usize] += 1;
        st.by_reason[(v & 0x3f) as usize] += 1;
    }
    for e in &old_edges.rows {
        st.edges_by_reason[e.reason as usize] -= 1;
    }
    for e in &fc.edges {
        st.edges_by_reason[e.reason as usize] += 1;
    }
    st.calls = info.len() as u64;
    st.edges = edges.len() as u64;
    class_stats(&mut st, &t);
    st.calls_time = t2.elapsed();
    let rc = ResolvedCalls { classes: t.classes, methods: t.methods, info, edges, edge_start, stats: st };
    (rc, CallsUpdate { affected, old_edges })
}

/// An empty resolution (a placeholder while the real one is updated in place).
pub(crate) fn empty_imports() -> Imports {
    Imports {
        modules: ModuleTable::default(),
        ns: Namespaces::default(),
        p1: Vec::new(),
        p2: Vec::new(),
        edges: Vec::new(),
        edge_start: vec![0],
        ext_edges: Vec::new(),
        ext_start: vec![0],
        getattr: None,
        stats: ImportStats::default(),
    }
}

/// The interned ids of every name the resolvers look up by text (builtins, dunders, marker
/// bases). Results can only be carried over while it is unchanged.
pub(crate) fn names_key(ix: &Index) -> Vec<Option<Sym>> {
    let s = ix.strings();
    let mut out: Vec<Option<Sym>> = super::builtins::texts::all().map(|n| s.get(n)).collect();
    out.extend(super::BUILTINS.iter().map(|n| s.get(n.as_bytes())));
    out.extend(super::BUILTIN_METHODS.iter().map(|n| s.get(n.as_bytes())));
    out
}

/// Prefix sums of `lens` (`lens.len() + 1` entries).
fn starts(lens: &[u32]) -> Vec<u32> {
    let mut s = Vec::with_capacity(lens.len() + 1);
    s.push(0u32);
    let mut at = 0u32;
    for &l in lens {
        at += l;
        s.push(at);
    }
    s
}
