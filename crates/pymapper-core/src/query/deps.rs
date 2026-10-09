//! Module dependencies of a target, in or out, rolled up to a level.
//!
//! The target is a subtree (a package, module, class or function); the other endpoint of every
//! edge is mapped to its representative at a [`Level`] and edges are aggregated per
//! representative. A representative never contains the target: when the level would map an
//! endpoint to an ancestor of the target (a sibling module rolled up to the shared package),
//! the endpoint's shallowest ancestor outside the target's ancestry is used instead.
//!
//! - Out: the call edges of the target's files with a caller in the target and a callee
//!   outside it, plus the module import edges of its files (and import rows inside a def
//!   target), plus imports of code that is not loaded ([`ExternalDep`]).
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
use crate::resolve::{Confidence, ExtKind, Reason, res_flags};
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
                for &f in &files {
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
                match external.iter_mut().find(|e| e.kind == x.kind && e.key == x.key) {
                    Some(e) => e.rows += x.rows,
                    None => external.push(ExternalDep { kind: x.kind, key: x.key, rows: x.rows }),
                }
            }
        }
    }

    /// Incoming import edges of the target's modules, from the reverse import graph.
    fn imports_in(&self, target: NodeId, opts: &DepOptions, agg: &mut Agg<'_>, importers: &mut Vec<u32>) {
        let ix = self.ix;
        let inside = |v: NodeId| ix.is_descendant(target, v);
        let mg = self.import_graph();
        let range = ix.subtree(target);
        let lo = mg.nodes().partition_point(|&v| v < range.start) as u32;
        let hi = mg.nodes().partition_point(|&v| v < range.end) as u32;
        importers.clear();
        for m in lo..hi {
            for &p in mg.graph().pred(m) {
                let v = mg.node(p);
                if !inside(v)
                    && let Some(f) = ix.file(v)
                {
                    importers.push(f);
                }
            }
        }
        importers.sort_unstable();
        importers.dedup();
        for &f in importers.iter() {
            let r = representative(ix, opts.level, target, ix.file_node(f));
            for me in self.imp.module_edges(f) {
                if inside(me.dst) && me.flags & opts.import_exclude == 0 {
                    agg.row(r).add_import(me.rows, me.flags);
                }
            }
        }
    }

    /// Files of the package/module nodes in `target`'s subtree (its file for a def).
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
        out
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
