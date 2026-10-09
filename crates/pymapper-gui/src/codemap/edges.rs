//! Edges of the semantic-zoom cuts with every edge metric (see [`super::zoom`]).
//!
//! For each band where the cut changes (ascending):
//!
//! 1. fill the cut map (one pre-order pass);
//! 2. [`Rollup::map_with`]: one row per cut pair `(A, B)` with call sites, distinct pairs,
//!    imports and confidence (one pass over the base tables);
//! 3. one pass over the base call pairs `(u, w, sites)` (sorted by `u`, so cut sources come
//!    in runs) adds the distinct-callee metrics: a stamp per callee (`seen[w] == run`)
//!    counts each callee once per source, which gives the API surface and the LOC reached;
//!    the complexity weight adds `sites x max(1, cc(w))` per pair;
//! 4. keep the rows *new* at this band (the band is the first of both endpoints' state
//!    intervals), pruned to the [`EdgeParams::per_source`] heaviest edges of each source
//!    under any metric.
//!
//! Cost per band: O(n) for the map plus O(m log d) for `m` base rows (binary search of the
//! cut destination within its source's run); bands are processed by a few workers in
//! parallel. Scratch per worker: 16 bytes per node. The output keeps only what the scene
//! may draw.

use rayon::prelude::*;

use pymapper_core::index::{Index, NodeId};
use pymapper_core::rollup::{Aggregates, LevelScratch, Rollup, RollupEdge};

use super::metrics::{EDGE_METRICS, EdgeMetric, EdgeWeights};
use super::zoom::Reveal;
use crate::map::scene::{Band, Lod};

/// One edge of the map: everything from `src` into `dst` in the bands of `lod` (36 bytes).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MapEdge {
    pub src: NodeId,
    pub dst: NodeId,
    pub lod: Lod,
    /// [`SRC_OPEN`] / [`DST_OPEN`]: the endpoint is shown open (it stands for its own code
    /// and is anchored at its header).
    pub flags: u8,
    /// Least sure call confidence (`Confidence as u8`), or [`NO_CALLS`].
    pub worst: u8,
    pub w: EdgeWeights,
}

pub const SRC_OPEN: u8 = 1;
pub const DST_OPEN: u8 = 2;
/// [`MapEdge::worst`] of an import-only edge.
pub const NO_CALLS: u8 = u8::MAX;

impl MapEdge {
    #[inline]
    pub fn weight(&self, m: EdgeMetric) -> u32 {
        self.w[m.column()]
    }
}

/// Edge computation parameters.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EdgeParams {
    /// Edges kept per source and band: the union of the heaviest under each metric
    /// (0 = keep all).
    pub per_source: usize,
    /// Parallel workers over bands (each holds 16 bytes per node of scratch).
    pub workers: usize,
}

impl Default for EdgeParams {
    fn default() -> EdgeParams {
        EdgeParams { per_source: 8, workers: 4 }
    }
}

/// Per-node inputs of the callee metrics.
struct Callee<'a> {
    loc: &'a [u32],
    cc: &'a [u32],
}

/// Scratch of one worker.
#[derive(Default)]
struct Scratch {
    map: Vec<NodeId>,
    level: LevelScratch,
    rows: Vec<RollupEdge>,
    /// `api, loc, complexity` per row of `rows`.
    extra: Vec<[u64; 3]>,
    seen: Vec<u32>,
    vals: Vec<u32>,
    keep: Vec<bool>,
}

/// The edges of every band of `reveal` (see the module docs), sorted by (first band, src,
/// dst).
pub fn compute(ix: &Index, agg: &Aggregates, rollup: &Rollup, reveal: &Reveal, params: EdgeParams) -> Vec<MapEdge> {
    let bands = reveal.change_bands();
    if bands.is_empty() {
        return Vec::new();
    }
    let callee = Callee { loc: agg.loc(), cc: agg.cc_sum() };
    let workers = params.workers.clamp(1, bands.len());
    let chunk = bands.len().div_ceil(workers);
    let parts: Vec<Vec<MapEdge>> = bands
        .par_chunks(chunk)
        .map(|bs| {
            let mut s = Scratch::default();
            let mut out = Vec::new();
            for &b in bs {
                band_edges(ix, rollup, reveal, &callee, b, params.per_source, &mut s, &mut out);
            }
            out
        })
        .collect();
    let mut all = Vec::with_capacity(parts.iter().map(Vec::len).sum());
    for p in parts {
        all.extend_from_slice(&p);
    }
    all
}

/// Appends the edges new at band `b` to `out`.
#[allow(clippy::too_many_arguments)]
fn band_edges(ix: &Index, rollup: &Rollup, reveal: &Reveal, callee: &Callee<'_>, b: Band, per_source: usize, s: &mut Scratch, out: &mut Vec<MapEdge>) {
    reveal.fill_cut(ix, b, &mut s.map);
    rollup.map_with(&s.map, &mut s.level, &mut s.rows);
    let rows = &s.rows;
    s.extra.clear();
    s.extra.resize(rows.len(), [0; 3]);
    s.seen.clear();
    s.seen.resize(s.map.len(), 0);
    let map = &s.map;
    // Run of the current cut source in `rows`, and its stamp in `seen`.
    let (mut cur, mut lo, mut hi, mut stamp) = (NodeId::MAX, 0usize, 0usize, 0u32);
    for (u, w, sites) in rollup.call_pairs() {
        let (a, bb) = (map[u as usize], map[w as usize]);
        if a == bb {
            continue;
        }
        if a != cur {
            cur = a;
            stamp += 1;
            lo = hi + rows[hi..].partition_point(|e| e.src < a);
            hi = lo + rows[lo..].partition_point(|e| e.src == a);
        }
        let Ok(k) = rows[lo..hi].binary_search_by_key(&bb, |e| e.dst) else {
            debug_assert!(false, "call pair without a cut row");
            continue;
        };
        let x = &mut s.extra[lo + k];
        if s.seen[w as usize] != stamp {
            s.seen[w as usize] = stamp;
            x[0] += 1;
            x[1] += u64::from(callee.loc[w as usize]);
        }
        x[2] += u64::from(sites) * u64::from(callee.cc[w as usize].max(1));
    }
    let sat = |x: u64| x.min(u64::from(u32::MAX)) as u32;
    // Prune per source run, then emit the rows new at this band.
    s.keep.clear();
    s.keep.resize(rows.len(), per_source == 0);
    let mut i = 0;
    while i < rows.len() {
        let mut j = i + 1;
        while j < rows.len() && rows[j].src == rows[i].src {
            j += 1;
        }
        if per_source > 0 {
            if j - i <= per_source {
                s.keep[i..j].fill(true);
            } else {
                for m in EdgeMetric::ALL {
                    s.vals.clear();
                    s.vals.extend((i..j).map(|r| weights(&rows[r], &s.extra[r], sat)[m.column()]));
                    let k = s.vals.len() - per_source;
                    let (_, t, _) = s.vals.select_nth_unstable(k);
                    let t = (*t).max(1);
                    for r in i..j {
                        if weights(&rows[r], &s.extra[r], sat)[m.column()] >= t {
                            s.keep[r] = true;
                        }
                    }
                }
            }
        }
        i = j;
    }
    for (r, e) in rows.iter().enumerate() {
        if !s.keep[r] {
            continue;
        }
        let (ea, eb) = (reveal.endpoint(e.src, b), reveal.endpoint(e.dst, b));
        if ea.lod.min.max(eb.lod.min) != b {
            continue;
        }
        let w = weights(e, &s.extra[r], sat);
        if w.iter().all(|&x| x == 0) {
            continue;
        }
        let flags = if ea.open { SRC_OPEN } else { 0 } | if eb.open { DST_OPEN } else { 0 };
        out.push(MapEdge {
            src: e.src,
            dst: e.dst,
            lod: Lod { min: b, max: ea.lod.max.min(eb.lod.max) },
            flags,
            worst: e.worst().map_or(NO_CALLS, |c| c as u8),
            w,
        });
    }
}

/// Every metric of one cut row.
#[inline]
fn weights(e: &RollupEdge, x: &[u64; 3], sat: impl Fn(u64) -> u32) -> EdgeWeights {
    let mut w = [0u32; EDGE_METRICS];
    w[EdgeMetric::Sites.column()] = e.sites;
    w[EdgeMetric::Pairs.column()] = e.pairs;
    w[EdgeMetric::LocReached.column()] = sat(x[1]);
    w[EdgeMetric::ApiSurface.column()] = sat(x[0]);
    w[EdgeMetric::Complexity.column()] = sat(x[2]);
    w[EdgeMetric::Imports.column()] = e.imports;
    w
}

/// Heap bytes of an edge table.
pub fn heap_bytes(edges: &[MapEdge]) -> usize {
    std::mem::size_of_val(edges)
}
