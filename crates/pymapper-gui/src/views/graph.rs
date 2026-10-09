//! Node-link graphs of the secondary views, cut out of the analysis:
//!
//! - [`level_graph`]: the code map's *current level*: the cells shown at the camera's band
//!   (the semantic-zoom cut of [`crate::codemap::zoom`]) that intersect the camera's view,
//!   with the rolled-up edges between them. Collapsed cells always appear; open cells (their
//!   children are shown) only when their own code has an edge.
//! - [`focus_graph`]: the N-hop neighbourhood of one node over the call graph (classes and
//!   functions) or the module graph (imports and calls rolled up to modules), callers and/or
//!   callees, heaviest edges first when the node budget runs out.
//!
//! Node lists are ascending [`NodeId`]s (pre-order), so a local id is a binary search and the
//! representative of any node (its deepest ancestor-or-self in the graph) costs O(depth log n).

use pymapper_core::index::{Index, NodeId};
use pymapper_core::rollup::{Level, LevelScratch, Rollup, RollupEdge};

use crate::codemap::CodeMap;
use crate::map::scene::{Band, WorldRect};

/// Edge metrics a view graph carries (the cheap rollup columns).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub enum Weight {
    /// Distinct call sites.
    #[default]
    Sites,
    /// Module import rows.
    Imports,
    /// Call sites plus import rows (every dependency).
    All,
}

impl Weight {
    pub const ALL: [Weight; 3] = [Weight::All, Weight::Imports, Weight::Sites];

    pub fn label(self) -> &'static str {
        match self {
            Weight::Sites => "call sites",
            Weight::Imports => "imports",
            Weight::All => "imports + call sites",
        }
    }
}

/// One directed edge between local ids (16 bytes).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ViewEdge {
    pub src: u32,
    pub dst: u32,
    pub sites: u32,
    pub imports: u32,
}

impl ViewEdge {
    fn of(src: u32, dst: u32, e: &RollupEdge) -> ViewEdge {
        ViewEdge { src, dst, sites: e.sites, imports: e.imports }
    }

    /// The weight under `w`.
    #[inline]
    pub fn weight(&self, w: Weight) -> u32 {
        match w {
            Weight::Sites => self.sites,
            Weight::Imports => self.imports,
            Weight::All => self.sites.saturating_add(self.imports),
        }
    }
}

/// A node-link graph over index nodes.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ViewGraph {
    /// Index node of every local id, ascending.
    pub nodes: Vec<NodeId>,
    /// Sorted by (src, dst); no self loops.
    pub edges: Vec<ViewEdge>,
    /// Hop distance from the focus (focus graphs; empty otherwise).
    pub hops: Vec<u8>,
    /// BFS parent of each node (focus graphs; the focus is its own parent).
    pub parent: Vec<u32>,
    /// Candidate nodes left out by the node budget.
    pub dropped: usize,
}

impl ViewGraph {
    /// Local id of `v`.
    #[inline]
    pub fn local(&self, v: NodeId) -> Option<u32> {
        self.nodes.binary_search(&v).ok().map(|i| i as u32)
    }

    /// Local id of the deepest ancestor-or-self of `v` in the graph.
    pub fn representative(&self, ix: &Index, v: NodeId) -> Option<u32> {
        if v >= ix.node_count() {
            return None;
        }
        let mut cur = Some(v);
        while let Some(c) = cur {
            if let Some(l) = self.local(c) {
                return Some(l);
            }
            cur = ix.parent(c);
        }
        None
    }

    /// Heap bytes (capacity).
    pub fn heap_bytes(&self) -> usize {
        4 * self.nodes.capacity() + 16 * self.edges.capacity() + self.hops.capacity() + 4 * self.parent.capacity()
    }
}

/// Keeps the rows of `rows` (sorted by (src, dst)) whose endpoints are both in `nodes`.
fn induced(nodes: &[NodeId], rows: &[RollupEdge], out: &mut Vec<ViewEdge>) {
    out.clear();
    for e in rows {
        if let (Ok(a), Ok(b)) = (nodes.binary_search(&e.src), nodes.binary_search(&e.dst)) {
            out.push(ViewEdge::of(a as u32, b as u32, e));
        }
    }
}

/// The current level of the code map (see the module docs) at band `band`, restricted to the
/// cells that intersect `view` (world rect of the code map), at most `max_nodes` cells (the
/// largest by lines of code win). Cost: O(visible part of the hierarchy) to collect cells,
/// O(n + m) for the rollup under the cut.
pub fn level_graph(cm: &CodeMap, band: Band, view: &WorldRect, max_nodes: usize) -> ViewGraph {
    let ix = cm.analysis.index();
    let (reveal, layout) = (&cm.reveal, &cm.layout);
    let n = ix.node_count();
    let mut leaves: Vec<NodeId> = Vec::new();
    let mut open: Vec<NodeId> = Vec::new();
    let mut v = 0;
    while v < n {
        let skip = ix.subtree(v).end;
        if reveal.rev[v as usize] > band {
            v = skip;
            continue;
        }
        let Some(r) = layout.rect(v) else {
            v = skip;
            continue;
        };
        let wr = WorldRect::new(r.x0, r.y0, r.x1, r.y1);
        if !(r.w() > 0.0 && r.h() > 0.0) || !wr.intersects(view) {
            v = skip;
            continue;
        }
        if reveal.open[v as usize] > band {
            leaves.push(v);
            v = skip;
        } else {
            open.push(v);
            v += 1;
        }
    }
    let mut dropped = 0;
    if leaves.len() > max_nodes {
        dropped = leaves.len() - max_nodes;
        let loc = cm.analysis.agg.loc();
        leaves.select_nth_unstable_by_key(max_nodes, |&v| (std::cmp::Reverse(loc[v as usize]), v));
        leaves.truncate(max_nodes);
        leaves.sort_unstable();
    }
    let mut map = Vec::new();
    reveal.fill_cut(ix, band, &mut map);
    let mut rows = Vec::new();
    cm.analysis.rollup.map_with(&map, &mut LevelScratch::default(), &mut rows);
    drop(map);
    // Open cells join only through edges of their own code (to kept cells or other open ones).
    let mut nodes = leaves;
    nodes.extend_from_slice(&open);
    nodes.sort_unstable();
    let mut edges = Vec::new();
    induced(&nodes, &rows, &mut edges);
    if !open.is_empty() {
        let mut used = vec![false; nodes.len()];
        for e in &edges {
            used[e.src as usize] = true;
            used[e.dst as usize] = true;
        }
        let keep: Vec<NodeId> = nodes.iter().zip(&used).filter(|&(v, &u)| u || open.binary_search(v).is_err()).map(|(&v, _)| v).collect();
        nodes = keep;
        induced(&nodes, &rows, &mut edges);
    }
    ViewGraph { nodes, edges, hops: Vec::new(), parent: Vec::new(), dropped }
}

/// Which neighbours the focus view follows.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub enum Direction {
    #[default]
    Both,
    /// Callers / importers (incoming edges).
    In,
    /// Callees / imported (outgoing edges).
    Out,
}

impl Direction {
    pub const ALL: [Direction; 3] = [Direction::Both, Direction::In, Direction::Out];

    pub fn label(self) -> &'static str {
        match self {
            Direction::Both => "both",
            Direction::In => "callers / importers",
            Direction::Out => "callees / imported",
        }
    }
}

/// Weighted forward and reverse adjacency over every index node (CSR; `u32` weights).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Adjacency {
    fwd_off: Vec<u32>,
    fwd: Vec<(u32, u32)>,
    rev_off: Vec<u32>,
    rev: Vec<(u32, u32)>,
}

impl Adjacency {
    /// From `(src, dst, weight)` rows over nodes `0..n`; `rows` is called twice (count, then
    /// scatter) and must yield the same rows both times, so nothing is materialised besides
    /// the two CSRs. Self loops are dropped. Rows in (src, dst) order without repeats give
    /// rows sorted by neighbour in both directions (required by [`Adjacency::weight`]).
    pub fn build<I>(n: u32, rows: impl Fn() -> I) -> Adjacency
    where
        I: Iterator<Item = (u32, u32, u32)>,
    {
        let n = n as usize;
        let mut fwd_off = vec![0u32; n + 1];
        let mut rev_off = vec![0u32; n + 1];
        for (s, d, _) in rows() {
            if s != d {
                fwd_off[s as usize + 1] += 1;
                rev_off[d as usize + 1] += 1;
            }
        }
        for i in 0..n {
            fwd_off[i + 1] += fwd_off[i];
            rev_off[i + 1] += rev_off[i];
        }
        let m = fwd_off[n] as usize;
        let (mut fwd, mut rev) = (vec![(0u32, 0u32); m], vec![(0u32, 0u32); m]);
        // Scatter cursors: each starts at its row's offset and ends at the next one's.
        let (mut fp, mut rp) = (fwd_off.clone(), rev_off.clone());
        for (s, d, w) in rows() {
            if s != d {
                fwd[fp[s as usize] as usize] = (d, w);
                fp[s as usize] += 1;
                rev[rp[d as usize] as usize] = (s, w);
                rp[d as usize] += 1;
            }
        }
        Adjacency { fwd_off, fwd, rev_off, rev }
    }

    /// The node-level call graph: one row per distinct (caller, callee) pair, weighted by
    /// call sites (the rollup's base pairs, already in (src, dst) order).
    pub fn calls(n: u32, rollup: &Rollup) -> Adjacency {
        Adjacency::build(n, || rollup.call_pairs())
    }

    /// The module graph: imports and calls rolled up to packages / modules, weighted by
    /// import rows plus call sites.
    pub fn modules(ix: &Index, rollup: &Rollup) -> Adjacency {
        let mut rows = Vec::new();
        rollup.level_with(ix, Level::Module, &mut LevelScratch::default(), &mut rows);
        Adjacency::build(ix.node_count(), || rows.iter().map(|e| (e.src, e.dst, e.sites.saturating_add(e.imports))))
    }

    #[inline]
    pub fn out(&self, v: NodeId) -> &[(u32, u32)] {
        let v = v as usize;
        if v + 1 >= self.fwd_off.len() {
            return &[];
        }
        &self.fwd[self.fwd_off[v] as usize..self.fwd_off[v + 1] as usize]
    }

    #[inline]
    pub fn inc(&self, v: NodeId) -> &[(u32, u32)] {
        let v = v as usize;
        if v + 1 >= self.rev_off.len() {
            return &[];
        }
        &self.rev[self.rev_off[v] as usize..self.rev_off[v + 1] as usize]
    }

    /// Weight of `a -> b` (0: no edge); binary search in `a`'s row.
    pub fn weight(&self, a: NodeId, b: NodeId) -> u32 {
        let row = self.out(a);
        debug_assert!(row.windows(2).all(|w| w[0].0 < w[1].0), "rows not sorted");
        row.binary_search_by_key(&b, |e| e.0).map_or(0, |k| row[k].1)
    }

    /// Heap bytes (capacity).
    pub fn heap_bytes(&self) -> usize {
        4 * (self.fwd_off.capacity() + self.rev_off.capacity()) + 8 * (self.fwd.capacity() + self.rev.capacity())
    }
}

/// Parameters of [`focus_graph`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct FocusParams {
    pub hops: u8,
    pub direction: Direction,
    /// Node budget, the focus included.
    pub max_nodes: usize,
}

impl Default for FocusParams {
    fn default() -> FocusParams {
        FocusParams { hops: 2, direction: Direction::Both, max_nodes: 400 }
    }
}

/// Whether a focus on `v` walks the call graph (classes and functions) rather than the
/// module graph (packages, modules, distributions).
#[inline]
pub fn focus_uses_calls(ix: &Index, v: NodeId) -> bool {
    ix.kind(v).is_def()
}

/// The `p.hops` neighbourhood of `focus` in `adj` (see the module docs). Hop by hop, the
/// candidates of the next ring are the unseen neighbours of the current ring, each with its
/// heaviest edge from it; when they exceed the remaining budget, the heaviest win (ties by
/// node id) and the rest count as dropped (a full budget ends the walk, so later rings are
/// not counted). The edges are every edge of `adj` between kept
/// nodes. Edge weights (`sites`) are the adjacency weights; `imports` is 0.
/// Cost: O(sum of the degrees of the expanded nodes + c log c) for `c` candidates.
pub fn focus_graph(adj: &Adjacency, focus: NodeId, p: FocusParams) -> ViewGraph {
    use pymapper_core::intern::FxHashMap;
    let max = p.max_nodes.max(1);
    let mut seen: FxHashMap<NodeId, (u8, NodeId)> = FxHashMap::default();
    seen.insert(focus, (0, focus));
    let mut frontier = vec![focus];
    let mut next = Vec::new();
    let mut cand: Vec<(u32, NodeId, NodeId)> = Vec::new();
    let mut dropped = 0;
    for hop in 1..=p.hops {
        cand.clear();
        for &u in &frontier {
            if p.direction != Direction::In {
                cand.extend(adj.out(u).iter().filter(|e| !seen.contains_key(&e.0)).map(|e| (e.1, e.0, u)));
            }
            if p.direction != Direction::Out {
                cand.extend(adj.inc(u).iter().filter(|e| !seen.contains_key(&e.0)).map(|e| (e.1, e.0, u)));
            }
        }
        // One candidate per node: its heaviest link into the ring (ties: lowest parent).
        cand.sort_unstable_by_key(|c| (c.1, std::cmp::Reverse(c.0), c.2));
        cand.dedup_by_key(|c| c.1);
        let room = max - seen.len();
        if cand.len() > room {
            dropped += cand.len() - room;
            if room > 0 {
                cand.select_nth_unstable_by_key(room - 1, |c| (std::cmp::Reverse(c.0), c.1));
            }
            cand.truncate(room);
        }
        next.clear();
        for &(_, v, parent) in &cand {
            seen.insert(v, (hop, parent));
            next.push(v);
        }
        std::mem::swap(&mut frontier, &mut next);
        if frontier.is_empty() || seen.len() >= max {
            break;
        }
    }
    let mut nodes: Vec<NodeId> = seen.keys().copied().collect();
    nodes.sort_unstable();
    let local = |v: NodeId| nodes.binary_search(&v).map(|i| i as u32);
    let mut hops = Vec::with_capacity(nodes.len());
    let mut parent = Vec::with_capacity(nodes.len());
    let mut edges = Vec::new();
    for (i, &u) in nodes.iter().enumerate() {
        let (h, par) = seen[&u];
        hops.push(h);
        parent.push(local(par).unwrap_or(i as u32));
        for &(w, wt) in adj.out(u) {
            if let Ok(j) = local(w) {
                edges.push(ViewEdge { src: i as u32, dst: j, sites: wt, imports: 0 });
            }
        }
    }
    ViewGraph { nodes, edges, hops, parent, dropped }
}

/// Radial layout of a focus graph: the focus at the origin, hop `k` on ring `k` (radius grows
/// with the ring's population so nodes keep about one unit apart), each ring ordered by its
/// parents' angles and then by node id (pre-order: siblings stay together). O(n log n).
pub fn radial_layout(g: &ViewGraph) -> Vec<[f32; 2]> {
    let n = g.nodes.len();
    let mut pos = vec![[0.0f32; 2]; n];
    let mut angle = vec![0.0f32; n];
    let max_hop = g.hops.iter().copied().max().unwrap_or(0);
    let mut ring: Vec<u32> = Vec::new();
    let mut radius = 0.0f32;
    for h in 1..=max_hop {
        ring.clear();
        ring.extend((0..n as u32).filter(|&i| g.hops[i as usize] == h));
        if ring.is_empty() {
            continue;
        }
        ring.sort_unstable_by(|&a, &b| {
            let (pa, pb) = (angle[g.parent[a as usize] as usize], angle[g.parent[b as usize] as usize]);
            pa.total_cmp(&pb).then(a.cmp(&b))
        });
        radius = (radius + 1.5).max(ring.len() as f32 * 1.2 / std::f32::consts::TAU);
        let step = std::f32::consts::TAU / ring.len() as f32;
        // Ring 1 starts at the top; later rings centre on their parents' span.
        let start = if h == 1 { -std::f32::consts::FRAC_PI_2 } else { angle[g.parent[ring[0] as usize] as usize] - 0.5 * step };
        for (k, &i) in ring.iter().enumerate() {
            let a = start + k as f32 * step;
            angle[i as usize] = a;
            pos[i as usize] = [radius * a.cos(), radius * a.sin()];
        }
    }
    pos
}
