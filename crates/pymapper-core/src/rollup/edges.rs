//! Edge rollups: base tables built once, then one linear pass per level.

use super::{Level, conf_of, merge_import_flags};
use crate::graph::CallFilter;
use crate::index::{Index, NONE, NodeId};
use crate::resolve::{CallEdge, Confidence, Imports, ResolvedCalls};

/// One edge of a level: everything from `src`'s subtree into `dst`'s subtree (24 bytes).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RollupEdge {
    /// Representative of the callers / importing modules.
    pub src: NodeId,
    /// Representative of the callees / imported modules.
    pub dst: NodeId,
    /// Distinct call sites (see the module docs).
    pub sites: u32,
    /// Distinct (caller, callee) node pairs.
    pub pairs: u32,
    /// Module import rows.
    pub imports: u32,
    /// [`res_flags`](crate::resolve::res_flags) shared by the import rows (valid when
    /// `imports > 0`).
    pub import_flags: u8,
    best: u8,
    worst: u8,
}

impl RollupEdge {
    #[inline]
    fn new(src: NodeId, dst: NodeId) -> RollupEdge {
        RollupEdge {
            src,
            dst,
            sites: 0,
            pairs: 0,
            imports: 0,
            import_flags: 0,
            best: Confidence::Unresolved as u8,
            worst: Confidence::Certain as u8,
        }
    }

    /// The import flag: at least one module import behind the edge.
    #[inline]
    pub fn has_import(&self) -> bool {
        self.imports > 0
    }

    /// At least one call pair behind the edge.
    #[inline]
    pub fn has_calls(&self) -> bool {
        self.pairs > 0
    }

    /// Most sure confidence of the call pairs (`None` without calls).
    #[inline]
    pub fn best(&self) -> Option<Confidence> {
        self.has_calls().then(|| conf_of(self.best))
    }

    /// Least sure confidence of the call pairs (`None` without calls).
    #[inline]
    pub fn worst(&self) -> Option<Confidence> {
        self.has_calls().then(|| conf_of(self.worst))
    }
}

#[cfg(test)]
impl RollupEdge {
    pub(super) fn new_for_tests(src: NodeId, dst: NodeId) -> RollupEdge {
        RollupEdge::new(src, dst)
    }

    pub(super) fn set_conf_for_tests(&mut self, best: u8, worst: u8) {
        self.best = best;
        self.worst = worst;
    }
}

/// A distinct (caller, callee) pair of the base (12 bytes).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct CallPair {
    src: NodeId,
    dst: NodeId,
    /// `sites << 4 | best << 2 | worst`.
    w: u32,
}

impl CallPair {
    #[inline]
    fn sites(self) -> u32 {
        self.w >> 4
    }

    #[inline]
    fn best(self) -> u8 {
        ((self.w >> 2) & 3) as u8
    }

    #[inline]
    fn worst(self) -> u8 {
        (self.w & 3) as u8
    }
}

/// Saturation of [`CallPair`] site counts.
const MAX_SITES: u32 = (1 << 28) - 1;

/// A module import edge of the base (12 bytes).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ImportPair {
    src: NodeId,
    dst: NodeId,
    /// `rows << 8 | flags`, rows saturated at `2^24 - 1`.
    w: u32,
}

/// A call site with kept edges to two or more distinct nodes; its targets are
/// `pool[off..off + len]` (12 bytes).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Group {
    src: NodeId,
    off: u32,
    len: u32,
}

/// Counters of [`Rollup::build`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RollupStats {
    /// Call edges kept by the filter (after merging repeated targets of one site).
    pub call_edges: u64,
    /// Distinct (caller, callee) pairs.
    pub call_pairs: u64,
    /// Call sites with at least one kept edge.
    pub sites: u64,
    /// Sites with kept edges to several distinct nodes.
    pub multi_sites: u64,
    /// Module import edges kept.
    pub import_edges: u64,
}

/// Base tables of the edge rollups (see the module docs). Build once, query any level.
#[derive(Clone, Debug, Default)]
pub struct Rollup {
    /// Sorted by (src, dst).
    calls: Vec<CallPair>,
    /// Sorted by (src, dst).
    imports: Vec<ImportPair>,
    /// Sorted by src.
    groups: Vec<Group>,
    pool: Vec<NodeId>,
    node_count: u32,
    stats: RollupStats,
}

/// Reusable scratch of [`Rollup::level_with`] (`8` bytes per node once warm).
#[derive(Clone, Debug, Default)]
pub struct LevelScratch {
    map: Vec<NodeId>,
    slot: Vec<u32>,
    bs: Vec<NodeId>,
}

impl LevelScratch {
    /// Heap bytes (capacity).
    pub fn heap_bytes(&self) -> usize {
        (self.map.capacity() + self.slot.capacity() + self.bs.capacity()) * 4
    }
}

/// Walks the call edges the filter keeps, in order. Edges of one site are consecutive: a
/// site's repeated targets are skipped. Calls `edge(src, dst, info)` per kept edge (`info`:
/// confidence in bits 0-1, bit 2 set when the edge has a site) and `group(src, targets)` after
/// every site with two or more distinct kept targets. Returns the number of sites with a kept
/// edge. A site has at most a few targets (constructor + `__init__` + `__new__`, at most 64
/// subclass methods), so the repeated-target check is a scan of the site's targets.
fn scan(
    edges: &[CallEdge],
    filter: CallFilter,
    dsts: &mut Vec<NodeId>,
    mut edge: impl FnMut(NodeId, NodeId, u8),
    mut group: impl FnMut(NodeId, &[NodeId]),
) -> u64 {
    let mut cur = (NONE, NONE);
    let mut sites = 0;
    dsts.clear();
    for e in edges {
        if !filter.keeps(e.conf, e.reason) {
            continue;
        }
        let has_site = e.site != NONE;
        if !has_site || (e.src, e.site) != cur {
            if dsts.len() > 1 {
                group(cur.0, dsts);
            }
            dsts.clear();
            cur = if has_site { (e.src, e.site) } else { (NONE, NONE) };
            sites += u64::from(has_site);
        }
        if has_site {
            if dsts.contains(&e.dst) {
                continue;
            }
            dsts.push(e.dst);
        }
        edge(e.src, e.dst, e.conf as u8 | (u8::from(has_site) << 2));
    }
    if dsts.len() > 1 {
        group(cur.0, dsts);
    }
    sites
}

impl Rollup {
    /// Builds the base tables from the call edges `filter` keeps (as
    /// [`call_graph`](crate::graph::call_graph)) and the module import edges whose flags do
    /// not intersect `import_exclude` (as [`import_graph`](crate::graph::import_graph)).
    ///
    /// Calls are bucketed by source with a counting sort (two passes over the edges, no
    /// comparison sort of the whole list), each bucket sorted by target and run-length
    /// encoded. Transient memory: `4 n + 8 m` bytes for `m` kept edges.
    pub fn build(ix: &Index, imp: &Imports, calls: &ResolvedCalls, filter: CallFilter, import_exclude: u8) -> Rollup {
        let n = ix.node_count() as usize;
        let edges = calls.edges();
        let mut dsts = Vec::new();
        // Pass 1: entries per source, then exclusive prefix sums (bucket starts).
        let mut start = vec![0u32; n];
        scan(edges, filter, &mut dsts, |src, _, _| start[src as usize] += 1, |_, _| {});
        let mut acc = 0u32;
        for s in &mut start {
            let c = *s;
            *s = acc;
            acc += c;
        }
        let m = acc as usize;
        // Pass 2: scatter `dst << 8 | info`; afterwards `start[v]` is the end of bucket `v`.
        let mut entries = vec![0u64; m];
        let mut groups = Vec::new();
        let mut pool = Vec::new();
        let sites = scan(
            edges,
            filter,
            &mut dsts,
            |src, dst, info| {
                let s = &mut start[src as usize];
                entries[*s as usize] = (u64::from(dst) << 8) | u64::from(info);
                *s += 1;
            },
            |src, targets| {
                groups.push(Group { src, off: pool.len() as u32, len: targets.len() as u32 });
                pool.extend_from_slice(targets);
            },
        );
        // Sort every bucket by target and count distinct targets, then encode exactly.
        let mut distinct = 0usize;
        let mut lo = 0usize;
        for &hi in &start {
            let b = &mut entries[lo..hi as usize];
            if b.len() > 1 {
                b.sort_unstable();
            }
            let mut last = u64::MAX;
            for &e in b.iter() {
                distinct += usize::from(e >> 8 != last);
                last = e >> 8;
            }
            lo = hi as usize;
        }
        let mut pairs = Vec::with_capacity(distinct);
        let mut lo = 0usize;
        for (v, &hi) in start.iter().enumerate() {
            let b = &entries[lo..hi as usize];
            let mut i = 0;
            while i < b.len() {
                let dst = (b[i] >> 8) as NodeId;
                let (mut ns, mut best, mut worst) = (0u32, 3u8, 0u8);
                while i < b.len() && (b[i] >> 8) as NodeId == dst {
                    let info = b[i] as u8;
                    ns += u32::from(info >> 2 & 1);
                    best = best.min(info & 3);
                    worst = worst.max(info & 3);
                    i += 1;
                }
                let w = (ns.min(MAX_SITES) << 4) | (u32::from(best) << 2) | u32::from(worst);
                pairs.push(CallPair { src: v as NodeId, dst, w });
            }
            lo = hi as usize;
        }
        drop(entries);
        drop(start);
        groups.sort_unstable_by_key(|g| g.src);
        groups.shrink_to_fit();
        pool.shrink_to_fit();

        let mut imports = Vec::with_capacity(imp.stats().module_edges as usize);
        for (f, e) in imp.all_module_edges() {
            let src = ix.file_node(f);
            if e.flags & import_exclude != 0 || src == NONE {
                continue;
            }
            let w = (e.rows.min((1 << 24) - 1) << 8) | u32::from(e.flags);
            imports.push(ImportPair { src, dst: e.dst, w });
        }
        imports.sort_unstable_by_key(|p| (p.src, p.dst));
        imports.shrink_to_fit();

        let stats = RollupStats {
            call_edges: m as u64,
            call_pairs: pairs.len() as u64,
            sites,
            multi_sites: groups.len() as u64,
            import_edges: imports.len() as u64,
        };
        Rollup { calls: pairs, imports, groups, pool, node_count: n as u32, stats }
    }

    #[inline]
    pub fn stats(&self) -> &RollupStats {
        &self.stats
    }

    /// Heap bytes of the base tables (capacity).
    pub fn heap_bytes(&self) -> usize {
        self.calls.capacity() * size_of::<CallPair>()
            + self.imports.capacity() * size_of::<ImportPair>()
            + self.groups.capacity() * size_of::<Group>()
            + self.pool.capacity() * 4
    }
}

impl Rollup {
    /// The edges of `level`. `ix` must be the index the rollup was built from.
    pub fn level(&self, ix: &Index, level: Level) -> LevelEdges {
        let mut edges = Vec::new();
        self.level_with(ix, level, &mut LevelScratch::default(), &mut edges);
        edges.shrink_to_fit();
        LevelEdges { level, edges }
    }

    /// The edges of `level` into `out` (cleared), sorted by (src, dst), reusing `scratch`
    /// and `out`'s capacity: no allocation once warm (see the module docs for the algorithm).
    ///
    /// # Panics
    /// If `ix` has a different node count than the index the rollup was built from.
    pub fn level_with(&self, ix: &Index, level: Level, scratch: &mut LevelScratch, out: &mut Vec<RollupEdge>) {
        assert_eq!(ix.node_count(), self.node_count, "rollup used with another index");
        let LevelScratch { map, slot, bs } = scratch;
        level.fill_map(ix, map);
        slot.resize(map.len(), 0);
        out.clear();
        let (calls, imports, groups) = (&self.calls[..], &self.imports[..], &self.groups[..]);
        let (mut i, mut j, mut g) = (0, 0, 0);
        while i < calls.len() || j < imports.len() {
            let ac = calls.get(i).map_or(NONE, |p| map[p.src as usize]);
            let ai = imports.get(j).map_or(NONE, |p| map[p.src as usize]);
            let a = ac.min(ai);
            let run = out.len();
            while i < calls.len() && map[calls[i].src as usize] == a {
                let p = calls[i];
                i += 1;
                let b = map[p.dst as usize];
                if b == a {
                    continue;
                }
                let r = find_or_push(out, slot, run, a, b);
                let r = &mut out[r];
                r.sites += p.sites();
                r.pairs += 1;
                r.best = r.best.min(p.best());
                r.worst = r.worst.max(p.worst());
            }
            while j < imports.len() && map[imports[j].src as usize] == a {
                let p = imports[j];
                j += 1;
                let b = map[p.dst as usize];
                if b == a {
                    continue;
                }
                let r = find_or_push(out, slot, run, a, b);
                let r = &mut out[r];
                let flags = p.w as u8;
                let acc = if r.imports == 0 { flags } else { r.import_flags };
                r.import_flags = merge_import_flags(acc, flags);
                r.imports += p.w >> 8;
            }
            // Multi-target sites of this run count once per destination. Every target of a
            // group is a call pair of the group's source, so its row is in this run.
            while g < groups.len() && map[groups[g].src as usize] == a {
                let gr = groups[g];
                g += 1;
                bs.clear();
                for &d in &self.pool[gr.off as usize..(gr.off + gr.len) as usize] {
                    let b = map[d as usize];
                    if b != a {
                        bs.push(b);
                    }
                }
                if bs.len() < 2 {
                    continue;
                }
                bs.sort_unstable();
                let mut k = 0;
                while k < bs.len() {
                    let b = bs[k];
                    let mut c = 0;
                    while k < bs.len() && bs[k] == b {
                        c += 1;
                        k += 1;
                    }
                    if c > 1 {
                        let s = slot[b as usize] as usize;
                        debug_assert!(s >= run && out[s].dst == b);
                        out[s].sites -= c - 1;
                    }
                }
            }
            if out.len() - run > 1 {
                out[run..].sort_unstable_by_key(|e| e.dst);
            }
        }
        debug_assert_eq!(g, groups.len(), "groups out of step with the call pairs");
    }
}

/// Row of destination `b` in the current run (rows `run..`, source `a`), pushed if new.
#[inline]
fn find_or_push(out: &mut Vec<RollupEdge>, slot: &mut [u32], run: usize, a: NodeId, b: NodeId) -> usize {
    let s = slot[b as usize] as usize;
    // Stale slots point before the run (or at a row of another destination): no clearing.
    if s >= run && s < out.len() && out[s].dst == b {
        return s;
    }
    slot[b as usize] = out.len() as u32;
    out.push(RollupEdge::new(a, b));
    out.len() - 1
}

/// The edges of one level, sorted by (src, dst).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LevelEdges {
    level: Level,
    edges: Vec<RollupEdge>,
}

impl LevelEdges {
    #[inline]
    pub fn level(&self) -> Level {
        self.level
    }

    /// Every edge, sorted by (src, dst).
    #[inline]
    pub fn edges(&self) -> &[RollupEdge] {
        &self.edges
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.edges.len()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.edges.is_empty()
    }

    /// Outgoing edges of representative `src`, sorted by dst (binary search).
    pub fn from(&self, src: NodeId) -> &[RollupEdge] {
        let lo = self.edges.partition_point(|e| e.src < src);
        let hi = lo + self.edges[lo..].partition_point(|e| e.src == src);
        &self.edges[lo..hi]
    }

    /// The edge `src -> dst`, if any (binary search).
    pub fn get(&self, src: NodeId, dst: NodeId) -> Option<&RollupEdge> {
        self.edges.binary_search_by_key(&(src, dst), |e| (e.src, e.dst)).ok().map(|i| &self.edges[i])
    }

    /// The edges, by value.
    pub fn into_vec(self) -> Vec<RollupEdge> {
        self.edges
    }

    /// Heap bytes (capacity).
    pub fn heap_bytes(&self) -> usize {
        self.edges.capacity() * size_of::<RollupEdge>()
    }
}
