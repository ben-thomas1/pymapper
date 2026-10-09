//! Module dependencies of a target, in or out, rolled up to a level.
//!
//! The target is a subtree (a package, module, class or function); the other endpoint of every
//! edge is mapped to its representative at a [`Level`] and edges are aggregated per
//! representative. A representative never contains the target: when the level would map an
//! endpoint to an ancestor of the target (a sibling module rolled up to the shared package),
//! the endpoint's shallowest ancestor outside the target's ancestry is used instead.
//!
//! - Out: the call edges of the target's files with a caller in the target and a callee
//!   outside it (override edges live in the overriding class's file: when the filter keeps
//!   them, the files of the target's callees are scanned too), plus the module import edges
//!   of its files (and import rows inside a def target), plus imports of code that is not
//!   loaded ([`ExternalDep`]).
//! - In: callers come from the reverse call graph (the files to scan are the files of the
//!   callers of the target's nodes), importers from the reverse import graph. A def target has
//!   no incoming import edges here (imports name modules).
//!
//! `sites` counts call sites as [`crate::rollup`] does: a site with several targets in one
//! representative (a constructor call linking the class and its `__init__`) counts once.
//! Aggregation goes through the scratch marks (slot per representative): no hashing, no sort
//! beyond the final ranking. Cost: O(target subtree + edges of the scanned files).

use super::{Query, Scratch};
use crate::graph::CallFilter;
use crate::index::{FileId, Index, NONE, NodeId};
use crate::intern::Sym;
use crate::resolve::{Confidence, ExtKind, ModuleEdge, Reason, res_flags};
use crate::roots::DistId;
use crate::rollup::Level;

/// Which side of the target.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Direction {
    /// What the target depends on.
    #[default]
    Out,
    /// What depends on the target.
    In,
}

/// Options of [`Query::deps`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DepOptions {
    /// Representative of the other endpoint (default: its package/module).
    pub level: Level,
    /// Call edges counted (default: every edge with a target except override edges, which are
    /// dynamic dispatch, not a dependency of the caller).
    pub calls: CallFilter,
    /// Count import edges.
    pub imports: bool,
    /// Import edges whose flags intersect this are skipped (`TYPE_ONLY` for runtime deps).
    pub import_exclude: u8,
}

impl Default for DepOptions {
    fn default() -> Self {
        DepOptions {
            level: Level::Module,
            calls: CallFilter::default().without(Reason::Override),
            imports: true,
            import_exclude: 0,
        }
    }
}

/// Everything between the target and one representative (24 bytes).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Dependency {
    pub node: NodeId,
    /// Distinct call sites.
    pub sites: u32,
    /// Call edges (site x callee; override edges included when the filter keeps them).
    pub calls: u32,
    /// Import rows.
    pub imports: u32,
    /// [`res_flags`] shared by the import rows (`EDGE_ALL` bits), plus `STAR` if any.
    pub import_flags: u8,
    best: u8,
    worst: u8,
}

impl Dependency {
    fn new(node: NodeId) -> Dependency {
        Dependency {
            node,
            sites: 0,
            calls: 0,
            imports: 0,
            import_flags: 0,
            best: Confidence::Unresolved as u8,
            worst: Confidence::Certain as u8,
        }
    }

    /// Most sure call edge (`None` without calls).
    pub fn best(&self) -> Option<Confidence> {
        (self.calls > 0).then(|| Confidence::from_u8(self.best))
    }

    /// Least sure call edge (`None` without calls).
    pub fn worst(&self) -> Option<Confidence> {
        (self.calls > 0).then(|| Confidence::from_u8(self.worst))
    }

    fn add_call(&mut self, conf: Confidence, new_site: bool) {
        self.calls += 1;
        self.sites += u32::from(new_site);
        self.best = self.best.min(conf as u8);
        self.worst = self.worst.max(conf as u8);
    }

    fn add_import(&mut self, rows: u32, flags: u8) {
        self.import_flags = if self.imports == 0 {
            flags & (res_flags::EDGE_ALL | res_flags::STAR)
        } else {
            (self.import_flags & flags & res_flags::EDGE_ALL) | ((self.import_flags | flags) & res_flags::STAR)
        };
        self.imports += rows;
    }

    /// Ranking weight: sites plus import rows.
    pub fn weight(&self) -> u64 {
        u64::from(self.sites) + u64::from(self.imports)
    }
}

/// Imports of code that is not loaded, per target (outgoing only).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExternalDep {
    pub kind: ExtKind,
    /// `Sym` (raw) of the top-level name for `Stdlib` / `Unknown`, the `DistId` for
    /// `ThirdParty`.
    pub key: u32,
    pub rows: u32,
}

impl ExternalDep {
    /// Top-level module name (`Stdlib` / `Unknown`).
    pub fn name<'a>(&self, ix: &'a Index) -> Option<&'a [u8]> {
        if self.kind == ExtKind::ThirdParty {
            return None;
        }
        Sym::from_raw(self.key).map(|s| ix.str(s))
    }

    /// The distribution (`ThirdParty`).
    pub fn dist(&self) -> Option<DistId> {
        (self.kind == ExtKind::ThirdParty).then_some(DistId(self.key))
    }
}

/// Result of [`Query::deps`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Deps {
    /// Heaviest first ([`Dependency::weight`], ties by node order).
    pub deps: Vec<Dependency>,
    /// Most rows first (outgoing only).
    pub external: Vec<ExternalDep>,
}

/// Aggregation through the scratch marks: `pos[rep]` = row in `out`.
struct Agg<'s> {
    marks: &'s mut super::Marks,
    out: Vec<Dependency>,
}

impl Agg<'_> {
    #[inline]
    fn row(&mut self, rep: NodeId) -> &mut Dependency {
        let i = match self.marks.get(rep) {
            Some(i) => i as usize,
            None => {
                self.marks.set(rep, self.out.len() as u32);
                self.out.push(Dependency::new(rep));
                self.out.len() - 1
            }
        };
        &mut self.out[i]
    }
}

/// Tracks the current call site so that a site with several targets mapping to one
/// representative counts once. Edges of one site are consecutive in the call edges.
#[derive(Default)]
struct SiteDedup {
    cur: (NodeId, u32),
    reps: Vec<NodeId>,
}

impl SiteDedup {
    /// Whether `(src, site)` is a new site for `rep` (override edges have no site: never).
    #[inline]
    fn new_site(&mut self, src: NodeId, site: u32, rep: NodeId) -> bool {
        if site == NONE {
            return false;
        }
        if (src, site) != self.cur {
            self.cur = (src, site);
            self.reps.clear();
        }
        if self.reps.contains(&rep) {
            return false;
        }
        self.reps.push(rep);
        true
    }
}

impl Query<'_> {
    /// Dependencies of `target` in direction `dir` (see the module docs).
    pub fn deps(&self, target: NodeId, dir: Direction, opts: &DepOptions, scratch: &mut Scratch) -> Deps {
        let ix = self.ix;
        let files = self.files_of(target);
        scratch.a.begin(ix.node_count() as usize);
        let mut agg = Agg { marks: &mut scratch.a, out: Vec::new() };
        let mut external: Vec<ExternalDep> = Vec::new();
        let mut dd = SiteDedup { cur: (NONE, NONE), reps: Vec::new() };
        let inside = |v: NodeId| ix.is_descendant(target, v);
        let rep = |w: NodeId| representative(ix, opts.level, target, w);
        match dir {
            Direction::Out => {
                // Override edges (base method -> override) live in the overriding class's file:
                // with a filter that keeps them, also scan the files of callees outside.
                let extra = &mut scratch.buf;
                extra.clear();
                if opts.calls.keeps(Confidence::Certain, Reason::Override) {
                    for v in ix.subtree(target) {
                        for &w in self.graph.succ(v) {
                            if !inside(w)
                                && let Some(f) = ix.file(w)
                                && files.binary_search(&f).is_err()
                            {
                                extra.push(f);
                            }
                        }
                    }
                    extra.sort_unstable();
                    extra.dedup();
                }
                for &f in files.iter().chain(extra.iter()) {
                    for e in self.calls.file_edges(f) {
                        if !opts.calls.keeps(e.conf, e.reason) || !inside(e.src) || inside(e.dst) {
                            continue;
                        }
                        let r = rep(e.dst);
                        let new = dd.new_site(e.src, e.site, r);
                        agg.row(r).add_call(e.conf, new);
                    }
                }
                if opts.imports {
                    self.imports_out(target, &files, opts, &mut agg, &mut external);
                }
            }
            Direction::In => {
                // Caller files: from the reverse call graph, outside the target.
                let callers = &mut scratch.buf;
                callers.clear();
                for v in ix.subtree(target) {
                    for &u in self.graph.pred(v) {
                        if !inside(u)
                            && let Some(f) = ix.file(u)
                        {
                            callers.push(f);
                        }
                    }
                }
                callers.sort_unstable();
                callers.dedup();
                let overrides = opts.calls.keeps(Confidence::Certain, Reason::Override);
                // Override edges live in the overriding class's file: the target's own files.
                let extra = files.iter().filter(|f| overrides && callers.binary_search(f).is_err());
                for &f in callers.iter().chain(extra) {
                    for e in self.calls.file_edges(f) {
                        if !opts.calls.keeps(e.conf, e.reason) || !inside(e.dst) || inside(e.src) {
                            continue;
                        }
                        // One unit on the target side: a site counts once whatever its targets.
                        let r = rep(e.src);
                        let new = dd.new_site(e.src, e.site, target);
                        agg.row(r).add_call(e.conf, new);
                    }
                }
                if opts.imports && !ix.kind(target).is_def() {
                    self.imports_in(target, opts, &mut agg, &mut scratch.queue);
                }
            }
        }
        let mut deps = agg.out;
        deps.sort_unstable_by(|a, b| b.weight().cmp(&a.weight()).then(a.node.cmp(&b.node)));
        external.sort_unstable_by(|a, b| b.rows.cmp(&a.rows).then((a.kind, a.key).cmp(&(b.kind, b.key))));
        Deps { deps, external }
    }

    /// Outgoing import edges of the target's files (import rows inside a def target).
    fn imports_out(&self, target: NodeId, files: &[FileId], opts: &DepOptions, agg: &mut Agg<'_>, external: &mut Vec<ExternalDep>) {
        let ix = self.ix;
        let inside = |v: NodeId| ix.is_descendant(target, v);
        let rep = |w: NodeId| representative(ix, opts.level, target, w);
        if ix.kind(target).is_def() {
            let Some(&f) = files.first() else { return };
            let scope = &ix.facts().imports.scope;
            for row in ix.file_facts(f).imports.range() {
                let dst = self.imp.module_node(row);
                let flags = self.imp.flags(row);
                if inside(ix.def_node(f, scope[row])) && dst != NONE && !inside(dst) && flags & opts.import_exclude == 0 {
                    agg.row(rep(dst)).add_import(1, flags);
                }
            }
            return;
        }
        for &f in files {
            for me in self.imp.module_edges(f) {
                if !inside(me.dst) && me.flags & opts.import_exclude == 0 {
                    agg.row(rep(me.dst)).add_import(me.rows, me.flags);
                }
            }
            for x in self.imp.ext_edges(f) {
                if x.flags & opts.import_exclude != 0 {
                    continue;
                }
                external.push(ExternalDep { kind: x.kind, key: x.key, rows: x.rows });
            }
        }
        // One entry per (kind, key): sort, then merge runs. O(k log k) for k edges.
        external.sort_unstable_by_key(|e| (e.kind, e.key));
        external.dedup_by(|later, kept| {
            let same = (later.kind, later.key) == (kept.kind, kept.key);
            if same {
                kept.rows += later.rows;
            }
            same
        });
    }

    /// Incoming import edges of the target's modules, from the reverse import graph.
    fn imports_in(&self, target: NodeId, opts: &DepOptions, agg: &mut Agg<'_>, importers: &mut Vec<u32>) {
        let ix = self.ix;
        let inside = |v: NodeId| ix.is_descendant(target, v);
        self.importer_files(target, importers);
        for &f in importers.iter() {
            let r = representative(ix, opts.level, target, ix.file_node(f));
            for me in self.imp.module_edges(f) {
                if inside(me.dst) && me.flags & opts.import_exclude == 0 {
                    agg.row(r).add_import(me.rows, me.flags);
                }
            }
        }
    }

    /// Files of the modules outside `target` importing a module in its subtree (from the
    /// reverse import graph), ascending, into `out`.
    fn importer_files(&self, target: NodeId, out: &mut Vec<u32>) {
        let ix = self.ix;
        let mg = self.import_graph();
        let range = ix.subtree(target);
        let lo = mg.nodes().partition_point(|&v| v < range.start) as u32;
        let hi = mg.nodes().partition_point(|&v| v < range.end) as u32;
        out.clear();
        for m in lo..hi {
            for &p in mg.graph().pred(m) {
                let v = mg.node(p);
                if !ix.is_descendant(target, v)
                    && let Some(f) = ix.file(v)
                {
                    out.push(f);
                }
            }
        }
        out.sort_unstable();
        out.dedup();
    }

    /// Files of the package/module nodes in `target`'s subtree (its file for a def), ascending.
    pub(super) fn files_of(&self, target: NodeId) -> Vec<FileId> {
        let ix = self.ix;
        if ix.kind(target).is_def() {
            return ix.file(target).into_iter().collect();
        }
        let mut out = Vec::new();
        let end = ix.subtree(target).end;
        let mut v = target;
        while v < end {
            if ix.kind(v).is_def() {
                v = ix.subtree(v).end;
                continue;
            }
            if let Some(f) = ix.file(v) {
                out.push(f);
            }
            v += 1;
        }
        out.sort_unstable();
        out
    }
}

/// Where an import dependency is written: an import row behind it (12 bytes).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ImportSite {
    pub file: FileId,
    /// 1-based line and column of the import row.
    pub line: u32,
    pub col: u32,
}

/// First rows of one dependency in scan order: `best` is the first whose own flags avoid
/// `import_exclude` (a runtime import under `--runtime`), `any` the first of all.
#[derive(Clone, Copy, Default)]
struct Pick {
    best: Option<(FileId, usize)>,
    any: Option<(FileId, usize)>,
}

impl Pick {
    #[inline]
    fn add(&mut self, f: FileId, row: usize, ok: bool) {
        if self.any.is_none() {
            self.any = Some((f, row));
        }
        if ok && self.best.is_none() {
            self.best = Some((f, row));
        }
    }

    #[inline]
    fn done(&self) -> bool {
        self.best.is_some()
    }
}

/// Whether file `f`'s module edge to `dst` (`edges`: its module edges) exists and is counted
/// with `exclude`.
#[inline]
fn edge_kept(edges: &[ModuleEdge], dst: NodeId, exclude: u8) -> bool {
    edges.binary_search_by_key(&dst, |e| e.dst).ok().is_some_and(|i| edges[i].flags & exclude == 0)
}

impl Query<'_> {
    /// The import row behind each of `deps` and `external`, which are rows (any subset) of
    /// [`Query::deps`] for the same `target`, `dir` and `opts`: parallel vectors, `None` for a
    /// row without import rows. The row is the dependency's first in (file, source) order:
    /// for outgoing rows in the target's files, for incoming rows in the importer's files. A
    /// row whose own flags intersect `opts.import_exclude` (a type-only row of an edge that
    /// `--runtime` keeps) is used only when no other row qualifies. One pass over the import
    /// rows of the files [`Query::deps`] scans for imports, with binary searches per row:
    /// O(R log E) for R rows and E edges of those files.
    pub fn import_sites(
        &self,
        target: NodeId,
        dir: Direction,
        opts: &DepOptions,
        deps: &[Dependency],
        external: &[ExternalDep],
        scratch: &mut Scratch,
    ) -> (Vec<Option<ImportSite>>, Vec<Option<ImportSite>>) {
        let ix = self.ix;
        let imp = self.imp;
        let inside = |v: NodeId| ix.is_descendant(target, v);
        let rep = |w: NodeId| representative(ix, opts.level, target, w);
        let exclude = opts.import_exclude;
        let mut picks = vec![Pick::default(); deps.len()];
        let mut ext_picks = vec![Pick::default(); external.len()];
        scratch.a.begin(ix.node_count() as usize);
        for (i, d) in deps.iter().enumerate() {
            if d.imports > 0 {
                scratch.a.set(d.node, i as u32);
            }
        }
        let mut ext_keys: Vec<(ExtKind, u32, u32)> =
            external.iter().enumerate().map(|(i, e)| (e.kind, e.key, i as u32)).collect();
        ext_keys.sort_unstable();
        let marks = &scratch.a;
        let is_def = ix.kind(target).is_def();
        match dir {
            Direction::Out if is_def => {
                // As `imports_out`: rows inside the def, counted (and excluded) row by row.
                if let Some(f) = ix.file(target) {
                    let scope = &ix.facts().imports.scope;
                    for row in ix.file_facts(f).imports.range() {
                        let dst = imp.module_node(row);
                        if dst == NONE || inside(dst) || imp.flags(row) & exclude != 0 || !inside(ix.def_node(f, scope[row])) {
                            continue;
                        }
                        if let Some(i) = marks.get(rep(dst)) {
                            picks[i as usize].add(f, row, true);
                        }
                    }
                }
            }
            Direction::Out => {
                for f in self.files_of(target) {
                    let edges = imp.module_edges(f);
                    for row in ix.file_facts(f).imports.range() {
                        let ok = imp.flags(row) & exclude == 0;
                        let dst = imp.module_node(row);
                        if dst != NONE
                            && !inside(dst)
                            && let Some(i) = marks.get(rep(dst))
                            && !picks[i as usize].done()
                            && edge_kept(edges, dst, exclude)
                        {
                            picks[i as usize].add(f, row, ok);
                        }
                        if ext_keys.is_empty() {
                            continue;
                        }
                        let Some(key) = imp.ext_key(row) else { continue };
                        let lo = ext_keys.partition_point(|k| (k.0, k.1) < key);
                        if let Some(&(_, _, i)) = ext_keys.get(lo).filter(|k| (k.0, k.1) == key)
                            && !ext_picks[i as usize].done()
                            && imp.ext_edges(f).iter().any(|x| (x.kind, x.key) == key && x.flags & exclude == 0)
                        {
                            ext_picks[i as usize].add(f, row, ok);
                        }
                    }
                }
            }
            // A def target has no incoming import edges.
            Direction::In if is_def => {}
            Direction::In => {
                let files = &mut scratch.buf;
                self.importer_files(target, files);
                for &f in files.iter() {
                    let Some(i) = marks.get(rep(ix.file_node(f))) else { continue };
                    let pick = &mut picks[i as usize];
                    let edges = imp.module_edges(f);
                    for row in ix.file_facts(f).imports.range() {
                        if pick.done() {
                            break;
                        }
                        let dst = imp.module_node(row);
                        if dst != NONE && inside(dst) && edge_kept(edges, dst, exclude) {
                            pick.add(f, row, imp.flags(row) & exclude == 0);
                        }
                    }
                }
            }
        }
        let st = &ix.facts().imports;
        let site = |p: &Pick| p.best.or(p.any).map(|(file, row)| ImportSite { file, line: st.line[row], col: st.col[row] + 1 });
        (picks.iter().map(site).collect(), ext_picks.iter().map(site).collect())
    }
}

/// Representative of `w` (outside `target`) at `level`, never an ancestor of `target` (see
/// the module docs).
fn representative(ix: &Index, level: Level, target: NodeId, w: NodeId) -> NodeId {
    let m = level.apply(ix, w);
    if !ix.is_descendant(m, target) {
        return m;
    }
    // `m` is an ancestor of the target: the shallowest ancestor of `w` that is not.
    for k in ix.depth(m) + 1..=ix.depth(w) {
        let a = ix.ancestor_at(w, k);
        if !ix.is_descendant(a, target) {
            return a;
        }
    }
    w
}
