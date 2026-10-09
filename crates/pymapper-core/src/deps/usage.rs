//! How much of each third-party distribution first-party code uses (per run, never cached).
//!
//! For every third-party group (and every distribution that first-party code imports but whose
//! code is not loaded):
//!
//! - `importers`: first-party files with an import of it; `import_rows`: those import rows.
//! - `modules_used`: distinct packages / modules of it imported directly by first-party code.
//! - `api_used`: distinct classes and functions of it referenced directly by first-party code:
//!   targets of `from m import name` rows and of call edges from first-party code.
//! - `call_sites`: call edges from first-party code into it (one per call site and target).
//! - `loc_reached` / `loc`: the lines of its code reachable from first-party code, and all its
//!   lines. Reachability is a breadth-first search from every node of the first-party files in
//!   scope over call edges at least as sure as [`UsageOptions::threshold`], import edges that
//!   are not type-only (importing a module runs its module-level code) and edges from a module
//!   to its parent package (importing `a.b` runs `a/__init__.py`). Lines are exclusive per node
//!   ([`Weights::of_index`]): a reached module counts its lines outside definitions, a reached
//!   class its body outside methods, a reached function its own lines.
//!
//! Cost: one pass over the first-party import rows and edges, one over the call edges, a
//! forward CSR of the union graph (4 bytes per node and per edge, forward only) and a BFS with
//! one bit per node. O(V + E).

use crate::index::{FileId, Index, NONE, NodeId, NodeKind};
use crate::intern::FxHashMap;
use crate::reach::{BitSet, Weights};
use crate::resolve::{Confidence, ExtKind, Imports, ResolvedCalls, res_flags};
use crate::roots::{DistId, Origin};

/// Options of [`usage`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UsageOptions {
    /// Least sure call edge followed and counted (`likely` by default: unique-method-name
    /// guesses would link first-party code to arbitrary libraries).
    pub threshold: Confidence,
}

impl Default for UsageOptions {
    fn default() -> Self {
        UsageOptions { threshold: Confidence::Likely }
    }
}

/// Usage of one distribution (see the module docs).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DistUsage {
    pub importers: u32,
    pub import_rows: u32,
    pub modules_used: u32,
    pub api_used: u32,
    pub call_sites: u32,
    pub loc_reached: u32,
    pub loc: u32,
}

impl DistUsage {
    /// Whether first-party code imports or calls it.
    pub fn is_used(&self) -> bool {
        self.importers > 0 || self.call_sites > 0
    }
}

/// Usage per group (third-party groups only are filled) and per distribution imported but not
/// loaded.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Usage {
    groups: Vec<DistUsage>,
    /// Distributions without a loaded group, ascending by id.
    unloaded: Vec<(DistId, DistUsage)>,
    /// First-party files in scope.
    pub first_party_files: u32,
    /// Nodes reached by the BFS (all origins).
    pub reached_nodes: u32,
}

impl Usage {
    /// Usage of group `g`.
    #[inline]
    pub fn group(&self, g: u32) -> &DistUsage {
        &self.groups[g as usize]
    }

    /// Distributions imported by first-party code whose code is not loaded.
    pub fn unloaded(&self) -> &[(DistId, DistUsage)] {
        &self.unloaded
    }
}

/// Group of every node's distribution, by distribution node.
fn dist_groups(ix: &Index) -> FxHashMap<NodeId, u32> {
    let mut m = FxHashMap::default();
    for g in 0..ix.groups().len() as u32 {
        m.insert(ix.group_node(g), g);
    }
    m
}

/// Group of node `v` (`NONE` if unknown).
#[inline]
fn group_of(ix: &Index, dg: &FxHashMap<NodeId, u32>, v: NodeId) -> u32 {
    match ix.file(v) {
        Some(f) => ix.file_group(f),
        None => dg.get(&ix.ancestor_at(v, 0)).copied().unwrap_or(NONE),
    }
}

/// A forward-only CSR over `0..n` from an edge source called twice (count, scatter).
struct Forward {
    off: Vec<u32>,
    adj: Vec<u32>,
}

impl Forward {
    fn build<I: Iterator<Item = (u32, u32)>>(n: u32, edges: impl Fn() -> I) -> Forward {
        // Counts at `off[s]`, inclusive prefix sums (`off[s]` = end of row `s`), then a
        // scatter that decrements each row's end down to its start: no second offsets array.
        let mut off = vec![0u32; n as usize + 1];
        for (s, _) in edges() {
            off[s as usize] += 1;
        }
        for i in 1..n as usize {
            off[i] += off[i - 1];
        }
        let m = if n == 0 { 0 } else { off[n as usize - 1] };
        off[n as usize] = m;
        let mut adj = vec![0u32; m as usize];
        for (s, d) in edges() {
            off[s as usize] -= 1;
            adj[off[s as usize] as usize] = d;
        }
        Forward { off, adj }
    }

    #[inline]
    fn succ(&self, v: u32) -> &[u32] {
        &self.adj[self.off[v as usize] as usize..self.off[v as usize + 1] as usize]
    }
}

/// Usage of every third-party distribution by the first-party files for which `in_scope`
/// holds (see the module docs).
pub fn usage(
    ix: &Index,
    imp: &Imports,
    calls: &ResolvedCalls,
    in_scope: impl Fn(FileId) -> bool,
    opts: &UsageOptions,
) -> Usage {
    let n = ix.node_count();
    let groups = ix.groups();
    let dg = dist_groups(ix);
    let third = |g: u32| g != NONE && groups[g as usize].origin == Origin::ThirdParty;
    let mut by_dist: FxHashMap<u32, u32> = FxHashMap::default();
    for (g, grp) in groups.iter().enumerate() {
        if let (Origin::ThirdParty, Some(d)) = (grp.origin, grp.dist) {
            by_dist.insert(d.0, g as u32);
        }
    }
    let mut out = Usage { groups: vec![DistUsage::default(); groups.len()], ..Usage::default() };
    let mut unloaded: FxHashMap<u32, DistUsage> = FxHashMap::default();
    // Directly used symbols and modules (one bit per node).
    let mut used = BitSet::new(n);
    // Last first-party file counted as an importer, per group / unloaded distribution.
    let mut last_importer = vec![NONE; groups.len()];
    let mut last_unloaded: FxHashMap<u32, FileId> = FxHashMap::default();
    let mut scope = BitSet::new(ix.file_count());
    for f in 0..ix.file_count() {
        if ix.file_origin(f) != Origin::FirstParty || !in_scope(f) {
            continue;
        }
        scope.insert(f);
        out.first_party_files += 1;
        for e in imp.module_edges(f) {
            let g = group_of(ix, &dg, e.dst);
            if !third(g) {
                continue;
            }
            let u = &mut out.groups[g as usize];
            u.import_rows += e.rows;
            if last_importer[g as usize] != f {
                last_importer[g as usize] = f;
                u.importers += 1;
            }
            if used.insert_new(e.dst) {
                u.modules_used += 1;
            }
        }
        for e in imp.ext_edges(f) {
            if e.kind != ExtKind::ThirdParty {
                continue;
            }
            // A distribution with loaded code that a row still attributes (a module of it
            // that is not loaded) counts on its group.
            let (u, last) = match by_dist.get(&e.key) {
                Some(&g) => (&mut out.groups[g as usize], &mut last_importer[g as usize]),
                None => (unloaded.entry(e.key).or_default(), last_unloaded.entry(e.key).or_insert(NONE)),
            };
            u.import_rows += e.rows;
            if *last != f {
                *last = f;
                u.importers += 1;
            }
        }
        for row in ix.file_facts(f).imports.range() {
            let Some(t) = imp.target(row) else { continue };
            if !ix.kind(t).is_def() {
                continue;
            }
            let g = group_of(ix, &dg, t);
            if third(g) && used.insert_new(t) {
                out.groups[g as usize].api_used += 1;
            }
        }
    }
    for e in calls.edges() {
        if e.conf > opts.threshold || e.conf == Confidence::Unresolved {
            continue;
        }
        let Some(sf) = ix.file(e.src) else { continue };
        if !scope.contains(sf) {
            continue;
        }
        let g = group_of(ix, &dg, e.dst);
        if !third(g) {
            continue;
        }
        let u = &mut out.groups[g as usize];
        if e.site != NONE {
            u.call_sites += 1;
        }
        if ix.kind(e.dst).is_def() && used.insert_new(e.dst) {
            u.api_used += 1;
        }
    }
    drop(used);
    reach_lines(ix, imp, calls, &scope, opts, &mut out);
    let mut un: Vec<(DistId, DistUsage)> = unloaded.into_iter().map(|(d, u)| (DistId(d), u)).collect();
    un.sort_unstable_by_key(|r| r.0.0);
    out.unloaded = un;
    out
}

/// Fills `loc` and `loc_reached` of every third-party group (see the module docs).
fn reach_lines(ix: &Index, imp: &Imports, calls: &ResolvedCalls, scope: &BitSet, opts: &UsageOptions, out: &mut Usage) {
    let n = ix.node_count();
    let threshold = opts.threshold;
    let edges = calls.edges();
    let g = Forward::build(n, || {
        let call = edges
            .iter()
            .filter(move |e| e.conf <= threshold && e.conf != Confidence::Unresolved)
            .map(|e| (e.src, e.dst));
        let import = imp
            .all_module_edges()
            .filter(|(_, e)| e.flags & res_flags::TYPE_ONLY == 0)
            .map(|(f, e)| (ix.file_node(f), e.dst))
            .filter(|&(s, _)| s != NONE);
        let parent = (0..ix.file_count()).filter_map(|f| {
            let v = ix.file_node(f);
            if v == NONE {
                return None;
            }
            let p = ix.parent(v)?;
            (ix.kind(p) == NodeKind::Package).then_some((v, p))
        });
        call.chain(import).chain(parent)
    });
    let mut live = BitSet::new(n);
    let mut queue: Vec<u32> = Vec::new();
    for f in scope.ones() {
        let v = ix.file_node(f);
        if v == NONE {
            continue;
        }
        for w in v..v + ix.file_facts(f).defs.len.max(1) {
            if live.insert_new(w) {
                queue.push(w);
            }
        }
    }
    let mut head = 0;
    while head < queue.len() {
        let v = queue[head];
        head += 1;
        for &w in g.succ(v) {
            if live.insert_new(w) {
                queue.push(w);
            }
        }
    }
    out.reached_nodes = queue.len() as u32;
    drop(queue);
    drop(g);
    let w = Weights::of_index(ix);
    for (gi, grp) in ix.groups().iter().enumerate() {
        if grp.origin != Origin::ThirdParty {
            continue;
        }
        let root = ix.group_node(gi as u32);
        let u = &mut out.groups[gi];
        for v in ix.subtree(root) {
            let lines = w.lines[v as usize];
            u.loc = u.loc.saturating_add(lines);
            if live.contains(v) {
                u.loc_reached = u.loc_reached.saturating_add(lines);
            }
        }
    }
}

#[cfg(test)]
mod forward_tests {
    use super::Forward;

    #[test]
    fn forward_csr_groups_successors_by_source() {
        let edges = [(2, 0), (0, 1), (2, 1), (0, 1), (3, 3)];
        let g = Forward::build(5, || edges.iter().copied());
        let sorted = |v: u32| {
            let mut s = g.succ(v).to_vec();
            s.sort_unstable();
            s
        };
        assert_eq!(sorted(0), [1, 1]);
        assert!(g.succ(1).is_empty());
        assert_eq!(sorted(2), [0, 1]);
        assert_eq!(sorted(3), [3]);
        assert!(g.succ(4).is_empty());
        assert_eq!(g.adj.len(), edges.len());
        let empty = Forward::build(0, std::iter::empty);
        assert_eq!((empty.off.as_slice(), empty.adj.len()), (&[0u32][..], 0));
        let no_edges = Forward::build(3, std::iter::empty);
        assert!((0..3).all(|v| no_edges.succ(v).is_empty()));
    }
}
