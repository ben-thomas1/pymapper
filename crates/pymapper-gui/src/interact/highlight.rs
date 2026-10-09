//! The interaction highlight: an overlay scene ([`crate::map::OVERLAY_HIGHLIGHT`]) built from
//! the hovered / selected item and the current path, over the main scene's geometry.
//!
//! ```text
//!   focus = hovered node, else the selected node
//!     veil over the map (dims everything drawn below)
//!     focus cell outlined; every drawn edge touching its subtree re-drawn on top, coloured
//!     by direction (out = callees, in = callers, internal), with the cell at its other end
//!     outlined in the same colour, both in exactly the bands the edge is drawn in
//!   hovered edge: both endpoint cells outlined
//!   selected node: outlined, plus a screen-size marker in bands where its cell is tiny
//!   path: every node outlined + marker, consecutive nodes joined by arcs
//! ```
//!
//! Everything is world geometry with LOD bands, so the overlay pans and zooms with no CPU
//! work; it is rebuilt only when the hovered or selected item or the path changes. Cost:
//! O(log E + k) for k touching edges (capped at [`HighlightSpec::max_edges`]).

use pymapper_core::index::{Index, NodeId};

use super::sceneindex::{Dir, NO_CELL, SceneIndex};
use crate::codemap::edges::MapEdge;
use crate::map::scene::{Band, LOD_UNBOUNDED, Lod, RectInstance, RectStyle, Rgba, Scene, SceneBuilder, WorldRect, band_for_zoom, band_zoom};

pub const VEIL: Rgba = Rgba::new(8, 10, 14, 120);
pub const OUT_COLOR: Rgba = Rgba::rgb(255, 170, 60);
pub const IN_COLOR: Rgba = Rgba::rgb(80, 200, 255);
pub const INTERNAL_COLOR: Rgba = Rgba::rgb(210, 210, 220);
pub const FOCUS_COLOR: Rgba = Rgba::rgb(255, 255, 255);
pub const SELECT_COLOR: Rgba = Rgba::rgb(0, 230, 200);
pub const PATH_COLOR: Rgba = Rgba::rgb(255, 80, 210);
/// Marker size in physical pixels (between 1x and 2x of this within a band).
pub const MARKER_PX: f32 = 14.0;

/// What the pointer is over.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Focus {
    Node(NodeId),
    /// An index into the code map's edge table.
    Edge(u32),
}

/// What to highlight.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HighlightSpec<'a> {
    pub hover: Option<Focus>,
    pub selected: Option<NodeId>,
    /// Nodes of a path, in order (empty: none).
    pub path: &'a [NodeId],
    /// Draw the veil when something is focused.
    pub dim: bool,
    /// Most touching edges re-drawn (the rest are counted only).
    pub max_edges: usize,
}

impl Default for HighlightSpec<'_> {
    fn default() -> Self {
        HighlightSpec { hover: None, selected: None, path: &[], dim: true, max_edges: 20_000 }
    }
}

/// Counts of what the highlight found (for the details panel).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HighlightStats {
    pub out_edges: u32,
    pub in_edges: u32,
    pub internal_edges: u32,
    /// Touching edges beyond [`HighlightSpec::max_edges`] (counted, not drawn).
    pub skipped: u32,
}

/// The scene the overlay is drawn over.
#[derive(Clone, Copy)]
pub struct Base<'a> {
    pub scene: &'a Scene,
    pub sx: &'a SceneIndex,
    pub edges: &'a [MapEdge],
    pub ix: &'a Index,
}

impl Base<'_> {
    /// The cell of `v`, or of its nearest drawn ancestor.
    pub fn cell_or_ancestor(&self, mut v: NodeId) -> Option<u32> {
        loop {
            let c = self.sx.cell(v);
            if c != NO_CELL {
                return Some(c);
            }
            v = self.ix.parent(v)?;
        }
    }

    fn rect(&self, cell: u32) -> &RectInstance {
        &self.scene.rects.inst[cell as usize]
    }
}

/// Builds the highlight overlay (an empty scene when there is nothing to show).
pub fn build(base: &Base<'_>, spec: &HighlightSpec<'_>) -> (Scene, HighlightStats) {
    let mut b = SceneBuilder::new();
    let mut stats = HighlightStats::default();
    let focus = spec.hover.or(spec.selected.map(Focus::Node));
    if focus.is_none() && spec.path.is_empty() {
        return (Scene::default(), stats);
    }
    if spec.dim && base.scene.bounds.is_positive() {
        let i = b.add_rect(0, 0, base.scene.bounds.expand(base.scene.bounds.width(), base.scene.bounds.height()), RectStyle { fill: VEIL, stroke: Rgba::TRANSPARENT, radius: 0.0, stroke_width: 0.0 }, None);
        b.set_rect_lod(i, Lod::ALWAYS);
    }
    match focus {
        Some(Focus::Node(v)) => neighbourhood(base, v, spec.max_edges, &mut b, &mut stats),
        Some(Focus::Edge(e)) => {
            if let Some(me) = base.edges.get(e as usize) {
                for (v, color) in [(me.src, OUT_COLOR), (me.dst, IN_COLOR)] {
                    if let Some(c) = base.cell_or_ancestor(v) {
                        outline(&mut b, base, c, color, 2.5, me.lod);
                    }
                }
                copy_segments(&mut b, base, e, FOCUS_COLOR, 1.5);
            }
        }
        None => {}
    }
    if let Some(s) = spec.selected
        && let Some(c) = base.cell_or_ancestor(s)
    {
        outline(&mut b, base, c, SELECT_COLOR, 3.0, Lod::ALWAYS);
        marker(&mut b, base, c, SELECT_COLOR);
    }
    path(&mut b, base, spec.path);
    (b.build(), stats)
}

/// Focus cell outline plus every drawn edge touching `v`'s subtree, with the far cells.
fn neighbourhood(base: &Base<'_>, v: NodeId, max_edges: usize, b: &mut SceneBuilder, stats: &mut HighlightStats) {
    let Some(fc) = base.cell_or_ancestor(v) else { return };
    let range = base.ix.subtree(v);
    let mut far: Vec<(NodeId, Band, Band, Dir)> = Vec::new();
    let mut drawn = 0usize;
    base.sx.edges_touching(base.edges, range, |e, dir| {
        match dir {
            Dir::Out => stats.out_edges += 1,
            Dir::In => stats.in_edges += 1,
            Dir::Internal => stats.internal_edges += 1,
        }
        if drawn >= max_edges {
            stats.skipped += 1;
            return;
        }
        drawn += 1;
        let me = &base.edges[e as usize];
        let color = dir_color(dir);
        copy_segments(b, base, e, color, 1.0);
        match dir {
            Dir::Out => far.push((me.dst, me.lod.min, me.lod.max, dir)),
            Dir::In => far.push((me.src, me.lod.min, me.lod.max, dir)),
            Dir::Internal => {}
        }
    });
    // One outline per (far node, bands, direction).
    far.sort_unstable();
    far.dedup();
    for &(n, lo, hi, dir) in &far {
        if let Some(c) = base.cell_or_ancestor(n) {
            outline(b, base, c, dir_color(dir), 2.0, Lod { min: lo, max: hi });
        }
    }
    outline(b, base, fc, FOCUS_COLOR, 2.5, Lod::ALWAYS);
}

pub fn dir_color(d: Dir) -> Rgba {
    match d {
        Dir::Out => OUT_COLOR,
        Dir::In => IN_COLOR,
        Dir::Internal => INTERNAL_COLOR,
    }
}

/// Re-draws the segments of edge `e` in `color`, `extra` points wider, in their own bands.
pub fn copy_segments(b: &mut SceneBuilder, base: &Base<'_>, e: u32, color: Rgba, extra: f32) {
    let curves = &base.scene.curves;
    for &s in base.sx.segments(e) {
        let inst = &curves.inst[s as usize];
        b.add_curve(e, [inst.p0, inst.c0, inst.c1, inst.p1], color, inst.width + extra, curves.lod[s as usize]);
    }
}

/// An outline of cell `cell` (transparent fill) in `lod`.
pub fn outline(b: &mut SceneBuilder, base: &Base<'_>, cell: u32, color: Rgba, width: f32, lod: Lod) {
    let r = base.rect(cell);
    let i = b.add_rect(base.scene.rects.id[cell as usize], 1, WorldRect { min: r.min, max: r.max }, RectStyle { fill: Rgba::TRANSPARENT, stroke: color, radius: r.radius, stroke_width: width }, None);
    let auto = b.rect_lod(i);
    // Never before the cell itself could show (an outline of an invisible speck is noise);
    // markers cover the bands before.
    b.set_rect_lod(i, Lod { min: lod.min.max(auto.min), max: lod.max });
}

/// Bands from which the whole map is a few hundred pixels wide (lowest band worth drawing a
/// marker in).
fn lowest_band(scene: &Scene) -> Band {
    let w = scene.bounds.width().max(scene.bounds.height());
    band_for_zoom(256.0 / w.max(1.0)).saturating_sub(1)
}

/// A screen-size marker on cell `cell`: in each band where the cell is smaller than
/// [`MARKER_PX`], a ring of `MARKER_PX`..2x physical pixels around its centre.
pub fn marker(b: &mut SceneBuilder, base: &Base<'_>, cell: u32, color: Rgba) {
    let r = base.rect(cell);
    let wr = WorldRect { min: r.min, max: r.max };
    let size = wr.width().max(wr.height());
    let until = Lod::for_size(size, MARKER_PX).min;
    let c = wr.center();
    let id = base.scene.rects.id[cell as usize];
    for band in lowest_band(base.scene)..until.min(LOD_UNBOUNDED - 1) {
        let h = MARKER_PX * 0.5 / band_zoom(band) as f32;
        let i = b.add_rect(id, 2, WorldRect::new(c[0] - h, c[1] - h, c[0] + h, c[1] + h), RectStyle { fill: Rgba::new(color.0[0], color.0[1], color.0[2], 70), stroke: color, radius: MARKER_PX, stroke_width: 2.0 }, None);
        b.set_rect_lod(i, Lod { min: band, max: band + 1 });
    }
}

/// A gently curved connector from `a` to `b` (control points offset to the left of the chord).
pub fn arc(a: [f32; 2], z: [f32; 2], bend: f32) -> [[f32; 2]; 4] {
    let d = [z[0] - a[0], z[1] - a[1]];
    let n = [-d[1] * bend, d[0] * bend];
    [a, [a[0] + d[0] / 3.0 + n[0], a[1] + d[1] / 3.0 + n[1]], [a[0] + 2.0 * d[0] / 3.0 + n[0], a[1] + 2.0 * d[1] / 3.0 + n[1]], z]
}

/// Outlines, markers and arcs of a path.
fn path(b: &mut SceneBuilder, base: &Base<'_>, nodes: &[NodeId]) {
    let mut prev: Option<[f32; 2]> = None;
    for &v in nodes {
        let Some(c) = base.cell_or_ancestor(v) else { continue };
        outline(b, base, c, PATH_COLOR, 2.5, Lod::ALWAYS);
        marker(b, base, c, PATH_COLOR);
        let r = base.rect(c);
        let p = WorldRect { min: r.min, max: r.max }.center();
        if let Some(q) = prev
            && q != p
        {
            b.add_curve(v, arc(q, p, 0.15), PATH_COLOR, 3.0, Lod::ALWAYS);
        }
        prev = Some(p);
    }
}
