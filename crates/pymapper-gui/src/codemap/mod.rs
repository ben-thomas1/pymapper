//! The primary code map: a squarified treemap of the hierarchy (sized by a selectable
//! metric, coloured by another), hierarchical edge bundling of the call or import graph
//! with a selectable edge weight, and semantic zoom that switches the rollup level per
//! subtree as cells grow on screen. GUI logic only: no GPU here, everything is testable.
//!
//! ```text
//!   load.rs     in-process analysis via core: Index, Imports, ResolvedCalls, Rollup, Aggregates
//!   metrics.rs  size / colour / edge metrics, colour ramps, legend data
//!   zoom.rs     reveal bands per node: the cut (rollup level) shown at each LOD band
//!   edges.rs    cut edges of every band with all edge metrics, pruned per source
//!   bundle.rs   hierarchical edge bundling: control polygon -> B-spline -> Bezier segments
//!   scene.rs    CodeMap -> map::Scene (rect and curve LODs from the reveal bands) + Legend
//!   worker.rs   background thread: load -> layout -> edges -> scenes on request; watch mode
//!   transition.rs  overlay animation of a watch-mode update (moved / changed / added / removed)
//! ```
//!
//! Data flow: [`load::load`] once, then [`CodeMap::build`] per size metric (layout, reveal
//! bands, edge table), then [`CodeMap::scene`] per colour / edge metric or bundling change.
//! The renderer shows the scene; navigation never comes back here (see [`crate::map`]).

pub mod bundle;
pub mod edges;
pub mod load;
pub mod metrics;
pub mod scene;
pub mod transition;
pub mod worker;
pub mod zoom;

#[cfg(test)]
mod tests;
#[cfg(test)]
mod watch_tests;

use std::fmt::Write as _;
use std::sync::Arc;
use std::time::{Duration, Instant};

use pymapper_core::index::NodeId;
use pymapper_core::layout::{Layout, LayoutConfig, LayoutTree, SizeMetric, TreeOptions, cache_path, load_levels, save_levels};
use pymapper_core::resolve::Confidence;

use crate::map::scene::Scene;
use edges::{EdgeParams, MapEdge, NO_CALLS};
use load::Analysis;
use metrics::{EdgeMetric, kind_label};
use scene::{Legend, SceneInput, SceneParams};
use zoom::Reveal;

/// Third-party and stdlib nodes below this depth never open for edges: edges into external
/// code stop at its packages (depth 1), which keeps a venv from adding millions of
/// function-level edges. Their rects still reveal normally.
pub const EXTERNAL_EDGE_DEPTH: u32 = 1;

/// Parameters of [`CodeMap::build`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MapParams {
    pub size: SizeMetric,
    pub layout: LayoutConfig,
    pub edges: EdgeParams,
    pub external_edge_depth: u32,
}

impl Default for MapParams {
    fn default() -> MapParams {
        MapParams { size: SizeMetric::Loc, layout: map_layout(), edges: EdgeParams::default(), external_edge_depth: EXTERNAL_EDGE_DEPTH }
    }
}

/// The layout config of the map: the core default with a taller header band, so top-level
/// names fit one label line at the fit zoom of a large map (the header is capped at a
/// fraction of each cell's height, so small cells are unaffected).
pub fn map_layout() -> LayoutConfig {
    LayoutConfig { header: 160.0, ..LayoutConfig::default() }
}

/// An analysed project laid out for one size metric, with its edge table.
pub struct CodeMap {
    pub analysis: Arc<Analysis>,
    pub layout: Layout,
    pub reveal: Reveal,
    /// Sorted by first band, then (src, dst).
    pub edges: Vec<MapEdge>,
    pub params: MapParams,
    /// Wall time per build phase.
    pub phases: Vec<(&'static str, Duration)>,
    /// Layout levels loaded from the on-disk cache.
    pub layout_from_cache: u32,
}

impl std::fmt::Debug for CodeMap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CodeMap").field("nodes", &self.reveal.len()).field("edges", &self.edges.len()).finish_non_exhaustive()
    }
}

impl CodeMap {
    /// Lays out `analysis` and computes the reveal bands and the edge table. Every level is
    /// laid out (a few milliseconds even for a venv); levels cached for the same hierarchy and
    /// config are loaded instead, and fresh ones saved.
    pub fn build(analysis: Arc<Analysis>, params: MapParams) -> CodeMap {
        let mut phases = Vec::new();
        let t = Instant::now();
        let ix = analysis.index();
        let tree = Arc::new(LayoutTree::from_index(ix, &analysis.agg, TreeOptions { metric: params.size, min_weight: 1 }));
        let path = analysis.cache_dir.as_deref().map(cache_path);
        let cached = path.as_deref().and_then(|p| load_levels(p, &tree, &params.layout).ok()).unwrap_or_default();
        let mut layout = Layout::with_levels(Arc::clone(&tree), params.layout, cached);
        let layout_from_cache = layout.computed();
        layout.ensure(tree.levels().saturating_sub(1));
        if layout.computed() > layout_from_cache
            && let Some(p) = &path
        {
            // A failed save only costs the next start a re-layout.
            let _ = save_levels(p, &tree, &params.layout, layout.levels());
        }
        phases.push(("layout", t.elapsed()));
        let t = Instant::now();
        let reveal = Reveal::new(ix, &layout);
        let edge_reveal = reveal.clamped(ix, params.external_edge_depth, |v| layout.muted(v));
        phases.push(("reveal bands", t.elapsed()));
        let t = Instant::now();
        let edges = edges::compute(ix, &analysis.agg, &analysis.rollup, &edge_reveal, params.edges);
        phases.push(("edges", t.elapsed()));
        CodeMap { analysis, layout, reveal, edges, params, phases, layout_from_cache }
    }

    /// The renderer scene for `p`, its legend, and the drawn edges (curve id = edge index).
    pub fn scene(&self, p: &SceneParams) -> (Scene, Legend, Vec<u32>) {
        let input = SceneInput { ix: self.analysis.index(), agg: &self.analysis.agg, layout: &self.layout, reveal: &self.reveal, edges: &self.edges };
        let mut ids = Vec::new();
        let (scene, legend) = scene::build(&input, self.params.size, p, &mut ids);
        (scene, legend, ids)
    }

    /// Heap bytes of the layout, reveal bands and edge table (not the analysis).
    pub fn heap_bytes(&self) -> usize {
        self.layout.heap_bytes() + self.layout.tree().heap_bytes() + 2 * self.reveal.len() + edges::heap_bytes(&self.edges)
    }

    /// Details of node `v` into `out` (cleared): qualified id, kind, metrics.
    pub fn describe_node(&self, v: NodeId, out: &mut String) {
        out.clear();
        let ix = self.analysis.index();
        if v >= ix.node_count() {
            return;
        }
        let a = self.analysis.agg.get(v);
        let _ = write!(out, "{}\n{}", ix.qid_string(v), kind_label(ix.kind(v)));
        if self.layout.muted(v) {
            out.push_str(" (external)");
        }
        let _ = write!(out, "\nlines {}  symbols {}\ncyclomatic sum {} max {}\ncognitive sum {} max {}", a.loc, a.symbols, a.cc_sum, a.cc_max, a.cog_sum, a.cog_max);
        if let Some(f) = ix.file(v) {
            let _ = write!(out, "\n{}", ix.file_path(f).display());
        }
    }

    /// Details of edge `e` (an index into [`CodeMap::edges`]) into `out` (cleared).
    pub fn describe_edge(&self, e: u32, out: &mut String) {
        out.clear();
        let Some(me) = self.edges.get(e as usize) else { return };
        let ix = self.analysis.index();
        let _ = write!(out, "{}\n-> {}", ix.qid_string(me.src), ix.qid_string(me.dst));
        for m in EdgeMetric::ALL {
            let _ = write!(out, "\n{}: {}", m.label(), me.weight(m));
        }
        if me.worst != NO_CALLS {
            let _ = write!(out, "\nleast sure call: {:?}", Confidence::from_u8(me.worst));
        }
    }
}
