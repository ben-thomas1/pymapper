//! Code map -> renderer [`Scene`]: one rect per laid-out node (layer = depth, LOD from the
//! reveal bands), one or more Bezier segments per bundled edge (LOD from the edge's cut
//! interval), and the legend of what is drawn.

use pymapper_core::index::{Index, NodeId};
use pymapper_core::layout::{Layout, Region, SizeMetric};
use pymapper_core::resolve::Confidence;
use pymapper_core::rollup::Aggregates;

use super::bundle::{Bundler, Cubic, DEFAULT_BETA};
use super::edges::{MapEdge, NO_CALLS};
use super::metrics::{ColorLegend, ColorMetric, Colorizer, EdgeMetric, darken, mix, muted};
use super::zoom::Reveal;
use crate::map::scene::{Band, LOD_UNBOUNDED, Lod, Rgba, RectStyle, Scene, SceneBuilder, WorldRect};

/// Edge width range in points.
pub const EDGE_MIN_PT: f32 = 0.75;
pub const EDGE_MAX_PT: f32 = 5.0;

/// What the scene shows.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SceneParams {
    pub color: ColorMetric,
    pub edge: EdgeMetric,
    /// Bundling strength (0 = straight, 1 = follow the hierarchy).
    pub beta: f32,
    pub show_edges: bool,
    /// Also draw edges between two third-party / stdlib nodes (edges from first-party code
    /// into external code are always drawn).
    pub external_edges: bool,
    /// Most edges first shown in one band (the heaviest win); bounds memory and fill.
    pub max_edges_per_band: usize,
    /// Confidence filter: call edges are drawn only if at least one call behind them is this
    /// sure or surer (`Confidence as u8`; `Unresolved` = no filter). Under the import edge
    /// metric nothing is filtered; under call metrics import-only edges are dropped too
    /// (harmless: they weigh nothing there).
    pub min_conf: u8,
    /// Origin filter: bit `Region as u8` set = cells and edges of that origin are drawn
    /// ([`ORIGINS_ALL`] = no filter).
    pub origins: u8,
}

/// Every origin (first-party, third-party, stdlib) in [`SceneParams::origins`].
pub const ORIGINS_ALL: u8 = 0b111;

/// Whether a node in `region` passes the origin mask.
#[inline]
pub fn origin_shown(origins: u8, region: Region) -> bool {
    origins & (1 << region as u8) != 0
}

impl Default for SceneParams {
    fn default() -> SceneParams {
        SceneParams { color: ColorMetric::Kind, edge: EdgeMetric::Sites, beta: DEFAULT_BETA, show_edges: true, external_edges: false, max_edges_per_band: 40_000, min_conf: Confidence::Unresolved as u8, origins: ORIGINS_ALL }
    }
}

/// What the legend explains (built with the scene).
#[derive(Clone, Debug, PartialEq)]
pub struct Legend {
    pub size: SizeMetric,
    pub color: ColorMetric,
    pub color_legend: ColorLegend,
    pub edge: EdgeMetric,
    /// Edge weight drawn at [`EDGE_MAX_PT`] (log scale down to 1 at [`EDGE_MIN_PT`]).
    pub edge_max: u32,
    /// Edges and Bezier segments in the scene.
    pub edges: usize,
    pub segments: usize,
    /// Cells (node rects) in the scene, label carriers not counted.
    pub rects: usize,
    /// The scene contains third-party or stdlib cells (drawn muted).
    pub has_muted: bool,
}

/// Inputs of a scene build (borrowed from the code map).
pub struct SceneInput<'a> {
    pub ix: &'a Index,
    pub agg: &'a Aggregates,
    pub layout: &'a Layout,
    pub reveal: &'a Reveal,
    pub edges: &'a [MapEdge],
}

/// Edge colours: tail (caller side) and head (callee side); imports in their own pair.
pub const CALL_TAIL: Rgba = Rgba::rgb(255, 200, 90);
pub const CALL_HEAD: Rgba = Rgba::rgb(255, 105, 150);
pub const IMPORT_TAIL: Rgba = Rgba::rgb(140, 205, 255);
pub const IMPORT_HEAD: Rgba = Rgba::rgb(195, 140, 255);

/// Builds the scene and its legend. `edge_ids` receives, per curve id, the index of its
/// edge in `input.edges` (curve ids are those indices already, so this is the list of drawn
/// edges in draw order).
pub fn build(input: &SceneInput<'_>, size: SizeMetric, p: &SceneParams, edge_ids: &mut Vec<u32>) -> (Scene, Legend) {
    let SceneInput { ix, agg, layout, reveal, edges } = *input;
    let colors = Colorizer::new(p.color, ix, agg);
    let tree = layout.tree();
    let computed = layout.computed();
    let mut rects = 0usize;
    for d in 0..computed {
        rects += tree.level(d).len();
    }
    // Edges to draw: positive weight, the heaviest per first band.
    edge_ids.clear();
    if p.show_edges {
        let ext = p.external_edges;
        let conf_filter = p.min_conf < Confidence::Unresolved as u8 && p.edge != EdgeMetric::Imports;
        let origins = p.origins;
        let keep = |e: &MapEdge| {
            if !ext && layout.muted(e.src) && layout.muted(e.dst) {
                return false;
            }
            if conf_filter && e.best() > p.min_conf {
                return false; // import-only edges (NO_CALLS) are not call edges either
            }
            origins == ORIGINS_ALL || (origin_shown(origins, layout.region_of(e.src)) && origin_shown(origins, layout.region_of(e.dst)))
        };
        select_edges(edges, p.edge, p.max_edges_per_band, keep, edge_ids);
    }
    // Cells plus up to two label carriers per opening node.
    let mut b = SceneBuilder::with_capacity(rects + rects / 4, edge_ids.len() * 4);
    let mut has_muted = false;
    let mut cells = 0usize;
    for d in 0..computed {
        let Some(level) = layout.level(d) else { break };
        for (slot, &v) in tree.level(d).iter().enumerate() {
            let r = level[slot];
            if !(r.w() > 0.0 && r.h() > 0.0) {
                continue;
            }
            if p.origins != ORIGINS_ALL && !origin_shown(p.origins, layout.region_of(v)) {
                continue;
            }
            cells += 1;
            let mute = layout.muted(v);
            has_muted |= mute;
            let mut fill = colors.color(ix, agg, v);
            if mute {
                fill = muted(fill);
            }
            let style = RectStyle {
                fill,
                stroke: darken(fill, 0.45),
                radius: (4.0 - d as f32).max(1.0),
                stroke_width: if d <= 1 { 1.0 } else { 0.75 },
            };
            let layer = d.min(255) as u8;
            let wr = WorldRect::new(r.x0, r.y0, r.x1, r.y1);
            let open = reveal.open[v as usize];
            if open == LOD_UNBOUNDED {
                // Never opens: the cell carries its own label.
                let i = b.add_rect(v, layer, wr, style, Some(ix.name(v)));
                let lod = reveal.rect_lod(v, b.rect_lod(i));
                b.set_rect_lod(i, lod);
                continue;
            }
            // Opens: the label moves from the cell (while collapsed) to the header band (once
            // the children show), so it never overlaps the children's labels. Both carriers
            // are transparent rects; the shader hides a label whose rect is too small.
            let i = b.add_rect(v, layer, wr, style, None);
            let lod = reveal.rect_lod(v, b.rect_lod(i));
            b.set_rect_lod(i, lod);
            if lod.min < open {
                let c = b.add_rect(v, layer, wr, RectStyle::label_only(fill), Some(ix.name(v)));
                b.set_rect_lod(c, Lod { min: lod.min, max: open });
            }
            let h = layout.config().header_band(&r);
            if h.h() > 0.0 && h.w() > 0.0 {
                let c = b.add_rect(v, layer, WorldRect::new(h.x0, h.y0, h.x1, h.y1), RectStyle::label_only(fill), Some(ix.name(v)));
                b.set_rect_lod(c, Lod { min: open.max(lod.min), max: LOD_UNBOUNDED });
            }
        }
    }
    // Edges: width and alpha on a log scale of the selected weight.
    let edge_max = edge_ids.iter().map(|&i| edges[i as usize].weight(p.edge)).max().unwrap_or(0);
    let log_max = (1.0 + edge_max as f32).ln().max(f32::MIN_POSITIVE);
    let (tail, head) = if p.edge == EdgeMetric::Imports { (IMPORT_TAIL, IMPORT_HEAD) } else { (CALL_TAIL, CALL_HEAD) };
    let mut bundler = Bundler::default();
    let mut segs: Vec<Cubic> = Vec::with_capacity(8);
    let mut segments = 0usize;
    let mut drawn = 0usize;
    for &ei in edge_ids.iter() {
        let e = &edges[ei as usize];
        segs.clear();
        let n = bundler.route(ix, layout, e.src, e.flags & super::edges::SRC_OPEN != 0, e.dst, e.flags & super::edges::DST_OPEN != 0, p.beta, &mut segs);
        if n == 0 {
            continue;
        }
        let t = (1.0 + e.weight(p.edge) as f32).ln() / log_max;
        let width = EDGE_MIN_PT + (EDGE_MAX_PT - EDGE_MIN_PT) * t;
        let mut alpha = 50.0 + 150.0 * t;
        if e.worst != NO_CALLS && e.worst >= 2 && p.edge != EdgeMetric::Imports {
            alpha *= 0.6; // only possible / unresolved calls behind the edge
        }
        if layout.muted(e.src) && layout.muted(e.dst) {
            alpha *= 0.5;
        }
        for (k, c) in segs.iter().enumerate() {
            let mut col = mix(tail, head, (k as f32 + 0.5) / n as f32);
            col.0[3] = alpha.round().clamp(0.0, 255.0) as u8;
            b.add_curve(ei, *c, col, width, e.lod);
        }
        segments += n;
        drawn += 1;
    }
    let legend = Legend {
        size,
        color: p.color,
        color_legend: colors.legend(),
        edge: p.edge,
        edge_max,
        edges: drawn,
        segments,
        rects: cells,
        has_muted,
    };
    (b.build(), legend)
}

/// Indices of the edges to draw into `out`: weight > 0 under `metric`, at most `per_band` per
/// first band (the heaviest; ties by index). The edge table is sorted by first band.
pub fn select_edges(edges: &[MapEdge], metric: EdgeMetric, per_band: usize, keep: impl Fn(&MapEdge) -> bool, out: &mut Vec<u32>) {
    let mut i = 0;
    while i < edges.len() {
        let band: Band = edges[i].lod.min;
        let start = out.len();
        let mut j = i;
        while j < edges.len() && edges[j].lod.min == band {
            if edges[j].weight(metric) > 0 && keep(&edges[j]) {
                out.push(j as u32);
            }
            j += 1;
        }
        if per_band > 0 && out.len() - start > per_band {
            let run = &mut out[start..];
            run.select_nth_unstable_by_key(per_band, |&k| (std::cmp::Reverse(edges[k as usize].weight(metric)), k));
            out.truncate(start + per_band);
            out[start..].sort_unstable();
        }
        i = j;
    }
}

/// The node of a rect id (rects are identified by node id).
#[inline]
pub fn rect_node(id: u32) -> NodeId {
    id
}
