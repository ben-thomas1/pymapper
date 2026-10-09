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
//! visits of a lookup include every hop). Results of files that are not affected are copied
//! with node ids and rows translated; a carried result that would point at a node that is
//! gone makes its file affected instead (a safety net; the property tests check that it never
//! changes the outcome).
//!
//! Global tables (module trie, namespaces, classes and MROs, method index) are rebuilt; they
//! are a small share of the resolution (see `docs/benchmarks.md`).
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
    Attribution, CallEdge, CallStats, ExtEdge, ImportStats, Imports, ModuleEdge, Reason, ResolvedCalls, edges,
};
use crate::index::{FileId, Index, NONE, NodeId};
use crate::intern::{FxHashMap, FxHashSet, Sym};
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
    let parts: Vec<Part<T>> = files
        .par_chunks(chunk.max(1))
        .map(|fs| {
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
        })
        .collect();
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
    /// Old run starts per file (`files + 1` entries, the last is the table length) of the
    /// import, call and decorator tables.
    pub imports: &'a [u32],
    pub calls: &'a [u32],
    pub decorators: &'a [u32],
}

impl Carry<'_> {
    #[inline]
    fn node(&self, v: NodeId) -> NodeId {
        if v == NONE { NONE } else { self.map.get(v as usize).copied().unwrap_or(NONE) }
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

/// Import resolution where only the `affected` files are resolved and every other file's
/// results come from `old` (translated with `carry`). With `old == None` every file must be
/// affected (a full tracked resolution). Returns the resolution and the dependency lists of
/// the affected files (in order, with lengths).
fn imports_with(
    ix: &Index,
    attr: &Attribution,
    affected: &[bool],
    old: Option<(&Imports, &Carry<'_>)>,
) -> (Imports, Vec<FileId>, Vec<FileId>, Vec<u32>) {
    let t0 = Instant::now();
    let (modules, ns) = timing::time("resolve.modules", || rayon::join(|| ModuleTable::build(ix), || Namespaces::build(ix)));
    let t1 = Instant::now();
    let getattr = ix.strings().get(b"__getattr__");
    let cx = Ctx { ix, mods: &modules, ns: &ns, getattr };
    let n = ix.file_count();
    let fresh = listed(affected);
    let new_start = |f: FileId| ix.file_facts(f).imports.start;
    let (p1, p2, deps, dep_lens) = timing::time("resolve.imports", || {
        let (p1_fresh, p1_lens, _, _) = par_files(&fresh, |_: &mut (), f, out: &mut Vec<ModRes>, _| {
            for row in ix.file_facts(f).imports.range() {
                out.push(cx.module_part(attr, f, row));
            }
        });
        let mut p1 = Vec::with_capacity(ix.facts().imports.len() as usize);
        let (mut k, mut at) = (0usize, 0usize);
        for f in 0..n {
            if affected[f as usize] {
                let l = p1_lens[k] as usize;
                p1.extend_from_slice(&p1_fresh[at..at + l]);
                at += l;
                k += 1;
            } else if let Some((o, c)) = old {
                p1.extend_from_slice(&o.p1[c.imports[f as usize] as usize..c.imports[f as usize + 1] as usize]);
            }
        }
        drop(p1_fresh);
        debug_assert_eq!(p1.len(), ix.facts().imports.len() as usize);
        let names = Names { cx: &cx, p1: &p1 };
        let (p2_fresh, p2_lens, deps, dep_lens) = par_files(&fresh, |sc: &mut Scratch, f, out: &mut Vec<NameRes>, log| {
            sc.logging = true;
            for row in ix.file_facts(f).imports.range() {
                out.push(names.row(f, row, sc));
            }
            log.append(&mut sc.log);
        });
        let mut p2 = Vec::with_capacity(p1.len());
        let (mut k, mut at) = (0usize, 0usize);
        for f in 0..n {
            if affected[f as usize] {
                let l = p2_lens[k] as usize;
                p2.extend_from_slice(&p2_fresh[at..at + l]);
                at += l;
                k += 1;
            } else if let Some((o, c)) = old {
                for r in &o.p2[c.imports[f as usize] as usize..c.imports[f as usize + 1] as usize] {
                    p2.push(NameRes { target: c.node(r.target), via: shift_row(r.via, c.imports, new_start), ..*r });
                }
            }
        }
        (p1, p2, deps, dep_lens)
    });
    let t2 = Instant::now();
    let ((edges, edge_start), (ext_edges, ext_start)) = timing::time("resolve.edges", || {
        let (e_fresh, e_lens, _, _) =
            par_files(&fresh, |_: &mut (), f, out: &mut Vec<ModuleEdge>, _| edges::file_edges(ix, &modules, &p1, &p2, f, out));
        let (x_fresh, x_lens, _, _) =
            par_files(&fresh, |_: &mut (), f, out: &mut Vec<ExtEdge>, _| edges::file_ext_edges(ix, &p1, &p2, f, out));
        let mut e = (Vec::with_capacity(e_fresh.len()), Vec::with_capacity(n as usize + 1));
        let mut x = (Vec::with_capacity(x_fresh.len()), Vec::with_capacity(n as usize + 1));
        e.1.push(0u32);
        x.1.push(0u32);
        let (mut k, mut ea, mut xa) = (0usize, 0usize, 0usize);
        for f in 0..n {
            if affected[f as usize] {
                let (el, xl) = (e_lens[k] as usize, x_lens[k] as usize);
                e.0.extend_from_slice(&e_fresh[ea..ea + el]);
                x.0.extend_from_slice(&x_fresh[xa..xa + xl]);
                ea += el;
                xa += xl;
                k += 1;
            } else if let Some((o, c)) = old {
                e.0.extend(o.module_edges(f).iter().map(|m| ModuleEdge { dst: c.node(m.dst), ..*m }));
                x.0.extend_from_slice(o.ext_edges(f));
            }
            e.1.push(e.0.len() as u32);
            x.1.push(x.0.len() as u32);
        }
        (e, x)
    });
    let t3 = Instant::now();
    let mut stats = ImportStats {
        rows: p2.len() as u64,
        module_edges: edges.len() as u64,
        ext_edges: ext_edges.len() as u64,
        modules: u64::from(modules.len()),
        build: t1 - t0,
        resolve: t2 - t1,
        edges: t3 - t2,
        ..ImportStats::default()
    };
    for r in &p2 {
        stats.by_status[r.status as usize] += 1;
    }
    let imp = Imports { modules, ns, p1, p2, edges, edge_start, ext_edges, ext_start, getattr, stats };
    (imp, fresh, deps, dep_lens)
}

/// [`super::resolve_imports`] that also records every file's dependencies.
pub(crate) fn resolve_imports_tracked(ix: &Index, attr: &Attribution) -> (Imports, Deps) {
    let all = vec![true; ix.file_count() as usize];
    let (imp, _, files, lens) = imports_with(ix, attr, &all, None);
    (imp, Deps { start: starts(&lens), files })
}

/// Re-resolves the imports of `ix` after an update (see the module docs): `dirty` marks the
/// re-extracted files. Returns the new resolution and which files were re-resolved; `deps` is
/// updated in place.
pub(crate) fn reresolve_imports(
    ix: &Index,
    attr: &Attribution,
    old: &Imports,
    deps: &mut Deps,
    dirty: &[bool],
    carry: &Carry<'_>,
) -> (Imports, Vec<bool>) {
    let n = ix.file_count() as usize;
    let mut affected = vec![false; n];
    deps.readers(dirty, &mut affected);
    // Safety net: a carried result must not point at a node or row that is gone.
    for (f, a) in affected.iter_mut().enumerate() {
        if *a {
            continue;
        }
        let rows = &old.p2[carry.imports[f] as usize..carry.imports[f + 1] as usize];
        *a = rows.iter().any(|r| {
            (r.target != NONE && carry.node(r.target) == NONE)
                || (r.via != NONE && dirty[carry.imports.partition_point(|&s| s <= r.via) - 1])
        });
    }
    let (imp, fresh, files, lens) = imports_with(ix, attr, &affected, Some((old, carry)));
    deps.replace(&fresh, &files, &lens);
    (imp, affected)
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

/// Call resolution where only the `affected` files are resolved (see [`imports_with`]).
/// Returns the resolution, the affected files, their dependency lists and the split of every
/// file's edges.
#[allow(clippy::type_complexity)]
fn calls_with(
    ix: &Index,
    imp: &Imports,
    t: CallTables,
    affected: &[bool],
    old: Option<(&ResolvedCalls, &Carry<'_>, &CallSplit)>,
) -> (ResolvedCalls, Vec<FileId>, Vec<FileId>, Vec<u32>, CallSplit) {
    let t2 = Instant::now();
    let fresh = listed(affected);
    let parts: Vec<CallPart> = timing::time("resolve.calls", || {
        let cx = Cx { ix, imp, classes: &t.classes, tables: &t.tables, names: t.names };
        fresh
            .par_chunks(chunk_size(fresh.len()).max(1))
            .map(|fs| {
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
            })
            .collect()
    });
    let n = ix.file_count() as usize;
    let mut info = Vec::with_capacity(ix.facts().calls.len() as usize);
    let mut edges: Vec<CallEdge> = Vec::new();
    let mut edge_start = Vec::with_capacity(n + 1);
    let mut split = Vec::with_capacity(n);
    let (mut deps, mut dep_lens) = (Vec::new(), Vec::new());
    edge_start.push(0u32);
    let calls_start = |f: FileId| ix.file_facts(f).calls.start;
    let dec_start = |f: FileId| ix.file_facts(f).decorators.start;
    let mut parts = parts.into_iter();
    let mut cur: Option<CallPart> = None;
    // Position in the current part: file, info row, edge.
    let (mut pf, mut pi, mut pe) = (0usize, 0usize, 0usize);
    for f in 0..n {
        if affected[f] {
            while cur.as_ref().is_none_or(|c| pf == c.out.edge_lens.len()) {
                let mut c = parts.next().expect("a part per affected file");
                deps.append(&mut c.deps);
                dep_lens.append(&mut c.dep_lens);
                cur = Some(c);
                (pf, pi, pe) = (0, 0, 0);
            }
            let c = cur.as_ref().expect("current part");
            let rows = ix.file_facts(f as FileId).calls.len as usize;
            info.extend_from_slice(&c.out.info[pi..pi + rows]);
            let el = c.out.edge_lens[pf] as usize;
            edges.extend_from_slice(&c.out.edges[pe..pe + el]);
            split.push(c.out.call_lens[pf]);
            pf += 1;
            pi += rows;
            pe += el;
        } else if let Some((o, c, osplit)) = old {
            info.extend_from_slice(&o.info[c.calls[f] as usize..c.calls[f + 1] as usize]);
            let k = osplit[f] as usize;
            for (i, e) in o.file_edges(f as FileId).iter().enumerate() {
                let site = if i < k {
                    shift_row(e.site, c.calls, calls_start)
                } else if e.reason == Reason::Override {
                    NONE
                } else {
                    shift_row(e.site, c.decorators, dec_start)
                };
                edges.push(CallEdge { src: c.node(e.src), dst: c.node(e.dst), site, ..*e });
            }
            split.push(osplit[f]);
        }
        edge_start.push(edges.len() as u32);
    }
    debug_assert_eq!(info.len(), ix.facts().calls.len() as usize);
    let t3 = Instant::now();
    let classes = t.classes;
    let mut stats = CallStats {
        calls: info.len() as u64,
        edges: edges.len() as u64,
        classes: u64::from(classes.len()),
        classes_time: t.t_classes,
        mro_time: t.t_mro,
        calls_time: t3 - t2,
        ..CallStats::default()
    };
    for &v in &info {
        stats.by_conf[(v >> 6) as usize] += 1;
        stats.by_reason[(v & 0x3f) as usize] += 1;
    }
    for e in &edges {
        stats.edges_by_reason[e.reason as usize] += 1;
    }
    for c in 0..classes.len() {
        let fl = classes.flags(c);
        stats.incomplete_classes += u64::from(fl & super::class_flags::INCOMPLETE != 0);
        stats.no_c3 += u64::from(fl & super::class_flags::NO_C3 != 0);
    }
    let rc = ResolvedCalls { classes, methods: t.methods, info, edges, edge_start, stats };
    (rc, fresh, deps, dep_lens, split)
}

/// [`super::resolve_calls`] that also records every file's dependencies and edge split.
pub(crate) fn resolve_calls_tracked(ix: &Index, imp: &Imports) -> (ResolvedCalls, Deps, CallSplit) {
    let t = call_tables(ix, imp);
    let all = vec![true; ix.file_count() as usize];
    let (rc, _, files, lens, split) = calls_with(ix, imp, t, &all, None);
    (rc, Deps { start: starts(&lens), files }, split)
}

/// Re-resolves the calls of `ix` after an update (see the module docs). `dirty` marks the
/// re-extracted files and the files whose import results changed; `touched` holds the def
/// names of the re-extracted files, before and after. `deps` and `split` are updated in place.
#[allow(clippy::too_many_arguments)]
pub(crate) fn reresolve_calls(
    ix: &Index,
    imp: &Imports,
    old: &ResolvedCalls,
    deps: &mut Deps,
    split: &mut CallSplit,
    dirty: &[bool],
    carry: &Carry<'_>,
    touched: &FxHashSet<Sym>,
) -> (ResolvedCalls, Vec<bool>) {
    let t = call_tables(ix, imp);
    let n = ix.file_count() as usize;
    let mut dirty = dirty.to_vec();
    // Classes whose MRO or flags changed (as nodes): their files are dirty.
    let mut old_class: FxHashMap<NodeId, u32> = FxHashMap::default();
    old_class.reserve(old.classes.len() as usize);
    for k in 0..old.classes.len() {
        let w = carry.node(old.classes.node(k));
        if w != NONE {
            old_class.insert(w, k);
        }
    }
    for c in 0..t.classes.len() {
        let w = t.classes.node(c);
        let Some(fw) = ix.file(w) else { continue };
        if dirty[fw as usize] {
            continue;
        }
        let same = old_class.get(&w).is_some_and(|&k| {
            let (om, nm) = (old.classes.mro(k), t.classes.mro(c));
            old.classes.flags(k) == t.classes.flags(c)
                && om.len() == nm.len()
                && om.iter().zip(nm).all(|(&a, &b)| carry.node(old.classes.node(a)) == t.classes.node(b))
        });
        if !same {
            dirty[fw as usize] = true;
        }
    }
    drop(old_class);
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
        for (f, a) in affected.iter_mut().enumerate() {
            if *a {
                continue;
            }
            let fv = ix.file_view(f as FileId);
            let ff = ix.file_facts(f as FileId);
            *a = ff.calls.range().any(|row| hit(fv.call_chain(row))) || ff.decorators.range().any(|row| hit(fv.decorator_chain(row)));
        }
    }
    // Safety net: carried edges must not point at nodes that are gone.
    for (f, a) in affected.iter_mut().enumerate() {
        if !*a {
            *a = old.file_edges(f as FileId).iter().any(|e| carry.node(e.src) == NONE || carry.node(e.dst) == NONE);
        }
    }
    let (rc, fresh, files, lens, new_split) = calls_with(ix, imp, t, &affected, Some((old, carry, split)));
    deps.replace(&fresh, &files, &lens);
    *split = new_split;
    (rc, affected)
}

/// Whether file `f`'s import results differ between `old` (translated) and `new`: phase-1
/// and phase-2 results of every row, module edges and external edges. The file's facts must
/// be unchanged (same rows).
pub(crate) fn import_results_changed(old: &Imports, new: &Imports, carry: &Carry<'_>, ix: &Index, f: FileId) -> bool {
    let (o0, o1) = (carry.imports[f as usize] as usize, carry.imports[f as usize + 1] as usize);
    let r = ix.file_facts(f).imports.range();
    if o1 - o0 != r.len() {
        return true;
    }
    let new_start = |g: FileId| ix.file_facts(g).imports.start;
    let p1_same = old.p1[o0..o1].iter().zip(&new.p1[r.clone()]).all(|(a, b)| {
        a.module == b.module && a.ext == b.ext && a.status == b.status && a.matched == b.matched
    });
    let p2_same = old.p2[o0..o1].iter().zip(&new.p2[r]).all(|(a, b)| {
        carry.node(a.target) == b.target && shift_row(a.via, carry.imports, new_start) == b.via && a.status == b.status && a.flags == b.flags
    });
    let (oe, ne) = (old.module_edges(f), new.module_edges(f));
    let edges_same = oe.len() == ne.len() && oe.iter().zip(ne).all(|(a, b)| carry.node(a.dst) == b.dst && a.rows == b.rows && a.flags == b.flags);
    !(p1_same && p2_same && edges_same && old.ext_edges(f) == new.ext_edges(f))
}

/// The interned ids of every name the resolvers look up by text (builtins, dunders, marker
/// bases). Results can only be carried over while it is unchanged.
pub(crate) fn names_key(ix: &Index) -> Vec<Option<Sym>> {
    const NAMES: &[&[u8]] = &[
        b"__getattr__", b"globals", b"exec", b"self", b"__init__", b"__new__", b"__call__", b"__class__", b"metaclass",
        b"object", b"Generic", b"Protocol", b"ABC",
    ];
    let s = ix.strings();
    let mut out: Vec<Option<Sym>> = NAMES.iter().map(|n| s.get(n)).collect();
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
