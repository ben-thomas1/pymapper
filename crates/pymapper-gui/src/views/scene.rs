//! Renderer scenes of the secondary views (the same instanced renderer as the code map):
//!
//! - node-link (force-directed graph and N-hop focus): one round rect per node (radius
//!   clamped by the shader to a circle) sized by lines of code and coloured like the code map,
//!   a transparent label carrier to its right (the shader's label LOD shows the name once the
//!   node is big enough), one straight line per edge, trimmed to the circles, offset sideways
//!   so the two directions of a pair stay apart, with a two-stroke arrowhead;
//! - matrix: a background, cyclic component blocks on the diagonal, the diagonal itself, one
//!   rect per marked cell (dependencies below the diagonal in blue, feedback above it in red,
//!   alpha by the log of the weight), row headers with qualified names, column headers with
//!   row numbers.
//!
//! Rect ids: node-link nodes and their label carriers carry the local node id, edges the edge
//! index. Matrix rects carry [`matrix_id`] (kind in the top bits).

use pymapper_core::index::NodeId;

use super::dsm::Dsm;
use super::graph::{ViewGraph, Weight};
use crate::codemap::CodeMap;
use crate::codemap::metrics::{ColorMetric, Colorizer, darken, mix, muted};
use crate::codemap::scene::{CALL_HEAD, CALL_TAIL, IMPORT_HEAD, IMPORT_TAIL};
use crate::map::scene::{Lod, RectInstance, RectStyle, Rgba, Scene, SceneBuilder, WorldRect};

/// Half side of the world box node-link scenes are scaled into (within the camera's f32
/// precision budget of about 2^14).
pub const WORLD: f32 = 8192.0;
/// Node radius range in layout units (edges are about 1-2 units long).
pub const NODE_MIN: f32 = 0.12;
pub const NODE_MAX: f32 = 0.5;
/// Edge width range in points.
pub const LINE_MIN_PT: f32 = 0.75;
pub const LINE_MAX_PT: f32 = 4.0;
/// Label carriers span from the node to this many radii to its right.
pub const LABEL_SPAN: f32 = 13.0;
/// Selection outline colour (the hover highlight is yellow).
pub const SELECT_COLOR: Rgba = Rgba::rgb(80, 220, 255);
/// Focus node stroke.
pub const FOCUS_STROKE: Rgba = Rgba::rgb(255, 255, 255);
/// Neutral edge colour for combined weights.
pub const DEP_COLOR: Rgba = Rgba::rgb(170, 190, 215);

/// A node-link view as drawn: world positions and radii per local node.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct NodeLink {
    pub pos: Vec<[f32; 2]>,
    pub radius: Vec<f32>,
}

impl NodeLink {
    /// Outline rect of node `i` for the selection overlay.
    pub fn mark(&self, i: u32) -> Option<RectInstance> {
        let (p, r) = (self.pos.get(i as usize)?, self.radius.get(i as usize)? * 1.4);
        Some(RectInstance { min: [p[0] - r, p[1] - r], max: [p[0] + r, p[1] + r], fill: Rgba::TRANSPARENT, stroke: SELECT_COLOR, radius: 1e6, stroke_width: 2.5 })
    }

    /// Heap bytes (capacity).
    pub fn heap_bytes(&self) -> usize {
        8 * self.pos.capacity() + 4 * self.radius.capacity()
    }
}

/// Options of a node-link scene.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct NodeLinkStyle {
    pub color: ColorMetric,
    pub weight: Weight,
    /// Local id drawn with the focus stroke.
    pub focus: Option<u32>,
}

/// Node radii in layout units: `NODE_MIN..NODE_MAX` by the square root of lines of code
/// relative to the largest node.
pub fn radii(cm: &CodeMap, g: &ViewGraph) -> Vec<f32> {
    let loc = cm.analysis.agg.loc();
    let max = g.nodes.iter().map(|&v| loc[v as usize]).max().unwrap_or(0).max(1) as f32;
    g.nodes.iter().map(|&v| NODE_MIN + (NODE_MAX - NODE_MIN) * (loc[v as usize] as f32 / max).sqrt()).collect()
}

/// Builds the node-link scene of `g` laid out at `layout` (layout units; scaled into
/// [`WORLD`]), with radii `r` (layout units).
pub fn node_link(cm: &CodeMap, g: &ViewGraph, layout: &[[f32; 2]], r: &[f32], s: &NodeLinkStyle) -> (Scene, NodeLink) {
    let ix = &cm.analysis.index;
    let agg = &cm.analysis.agg;
    let n = g.nodes.len();
    debug_assert!(layout.len() == n && r.len() == n);
    let mut ext = 0.0f32;
    for (p, &ri) in layout.iter().zip(r) {
        // Label carriers reach LABEL_SPAN radii to the right of their node.
        ext = ext.max(p[0].abs() + ri).max(p[1].abs() + ri).max(p[0] + LABEL_SPAN * ri);
    }
    let scale = WORLD / ext.max(1.0);
    let nl = NodeLink { pos: layout.iter().map(|p| [p[0] * scale, p[1] * scale]).collect(), radius: r.iter().map(|&x| x * scale).collect() };
    let colors = Colorizer::new(s.color, ix, agg);
    let mut b = SceneBuilder::with_capacity(2 * n, 0);
    for (i, &v) in g.nodes.iter().enumerate() {
        let ([x, y], rr) = (nl.pos[i], nl.radius[i]);
        let mut fill = colors.color(ix, agg, v);
        if cm.layout.muted(v) {
            fill = muted(fill);
        }
        let focus = s.focus == Some(i as u32);
        let style = RectStyle {
            fill,
            stroke: if focus { FOCUS_STROKE } else { darken(fill, 0.45) },
            radius: 1e6,
            stroke_width: if focus { 3.0 } else { 1.0 },
        };
        let k = b.add_rect(i as u32, 1, WorldRect::new(x - rr, y - rr, x + rr, y + rr), style, None);
        b.set_rect_lod(k, Lod::ALWAYS);
        // The label to the right of the node, shown once one line fits the node's height. It
        // sits below the nodes (hit tests try upper layers first, so a node beats a
        // neighbour's label).
        let c = b.add_rect(i as u32, 0,WorldRect::new(x + 1.1 * rr, y - rr, x + LABEL_SPAN * rr, y + rr), RectStyle::label_only(Rgba::rgb(20, 22, 26)), Some(ix.name(v)));
        b.set_rect_lod(c, Lod::ALWAYS);
    }
    let max_w = g.edges.iter().map(|e| e.weight(s.weight)).max().unwrap_or(0);
    let log_max = (1.0 + max_w as f32).ln().max(f32::MIN_POSITIVE);
    let (tail, head) = match s.weight {
        Weight::Imports => (IMPORT_TAIL, IMPORT_HEAD),
        Weight::Sites => (CALL_TAIL, CALL_HEAD),
        Weight::All => (DEP_COLOR, DEP_COLOR),
    };
    for (ei, e) in g.edges.iter().enumerate() {
        let w = e.weight(s.weight);
        if w == 0 {
            continue;
        }
        let (a, c) = (e.src as usize, e.dst as usize);
        let (pa, pc, ra, rc) = (nl.pos[a], nl.pos[c], nl.radius[a], nl.radius[c]);
        let (dx, dy) = (pc[0] - pa[0], pc[1] - pa[1]);
        let len = (dx * dx + dy * dy).sqrt();
        if len <= ra + rc {
            continue; // overlapping nodes: nothing to draw between them
        }
        let (ux, uy) = (dx / len, dy / len);
        // Sideways offset (to the right of the direction): a <-> b stays two lines.
        let off = 0.2 * ra.min(rc);
        let (ox, oy) = (-uy * off, ux * off);
        let p0 = [pa[0] + ux * ra + ox, pa[1] + uy * ra + oy];
        let p1 = [pc[0] - ux * rc + ox, pc[1] - uy * rc + oy];
        let t = (1.0 + w as f32).ln() / log_max;
        let width = LINE_MIN_PT + (LINE_MAX_PT - LINE_MIN_PT) * t;
        let mut col = mix(tail, head, 0.5 + 0.5 * t);
        col.0[3] = (70.0 + 160.0 * t).round() as u8;
        b.add_line(ei as u32, p0, p1, col, width, Lod::ALWAYS);
        // Arrowhead: two strokes back from the tip.
        let al = (0.6 * rc).min(0.3 * (len - ra - rc)).max(0.0);
        if al > 0.0 {
            let (cs, sn) = (0.45f32.cos(), 0.45f32.sin());
            for sgn in [-1.0f32, 1.0] {
                let (bx, by) = (-ux * cs + sgn * uy * sn, -uy * cs - sgn * ux * sn);
                b.add_line(ei as u32, p1, [p1[0] + bx * al, p1[1] + by * al], col, width, Lod::ALWAYS);
            }
        }
    }
    (b.build(), nl)
}

/// Matrix rect id kinds (top three bits) and the index mask.
pub const ID_CELL: u32 = 0;
pub const ID_ROW: u32 = 1;
pub const ID_COL: u32 = 2;
pub const ID_BLOCK: u32 = 3;
pub const ID_BACK: u32 = 4;
const ID_SHIFT: u32 = 29;
const ID_MASK: u32 = (1 << ID_SHIFT) - 1;

/// The rect id of matrix item `index` of `kind`.
#[inline]
pub fn matrix_id(kind: u32, index: u32) -> u32 {
    kind << ID_SHIFT | (index & ID_MASK)
}

/// `(kind, index)` of a matrix rect id.
#[inline]
pub fn matrix_item(id: u32) -> (u32, u32) {
    (id >> ID_SHIFT, id & ID_MASK)
}

/// Matrix geometry: cell side and header sizes in world units.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MatrixGeom {
    pub n: u32,
    pub cell: f32,
    /// Width of the row headers (left of the matrix).
    pub header: f32,
    /// Height of the column headers (above the matrix).
    pub col_header: f32,
}

impl MatrixGeom {
    /// Cells of at most 64 units, the whole matrix within about 16k units.
    pub fn new(n: u32) -> MatrixGeom {
        let cell = (14_000.0 / n.max(1) as f32).min(64.0);
        MatrixGeom { n, cell, header: 24.0 * cell, col_header: 2.0 * cell }
    }

    /// Rect of row `r`'s band across headers and cells (selection overlay).
    pub fn row_band(&self, r: u32) -> WorldRect {
        let y = r as f32 * self.cell;
        WorldRect::new(-self.header, y, self.n as f32 * self.cell, y + self.cell)
    }

    /// Rect of column `c`'s band across headers and cells.
    pub fn col_band(&self, c: u32) -> WorldRect {
        let x = c as f32 * self.cell;
        WorldRect::new(x, -self.col_header, x + self.cell, self.n as f32 * self.cell)
    }

    /// Row and column bands of row `r` as selection overlays.
    pub fn marks(&self, r: u32) -> [RectInstance; 2] {
        let mk = |w: WorldRect| RectInstance { min: w.min, max: w.max, fill: Rgba::new(80, 220, 255, 36), stroke: SELECT_COLOR, radius: 0.0, stroke_width: 1.5 };
        [mk(self.row_band(r)), mk(self.col_band(r))]
    }
}

pub const BACK_COLOR: Rgba = Rgba::rgb(34, 37, 43);
pub const DIAG_COLOR: Rgba = Rgba::rgb(70, 75, 84);
pub const BLOCK_COLOR: Rgba = Rgba::new(220, 70, 60, 70);
pub const BELOW_COLOR: Rgba = Rgba::rgb(90, 160, 255);
pub const ABOVE_COLOR: Rgba = Rgba::rgb(255, 90, 70);

/// Builds the matrix scene of `d`.
pub fn matrix(cm: &CodeMap, d: &Dsm) -> (Scene, MatrixGeom) {
    let ix = &cm.analysis.index;
    let n = d.nodes.len() as u32;
    let g = MatrixGeom::new(n);
    let c = g.cell;
    let side = n as f32 * c;
    let mut b = SceneBuilder::with_capacity(3 * n as usize + d.cells.len() + d.blocks.len() + 1, 0);
    let always = |b: &mut SceneBuilder, k: usize| b.set_rect_lod(k, Lod::ALWAYS);
    if n > 0 {
        let k = b.add_rect(matrix_id(ID_BACK, 0), 0, WorldRect::new(0.0, 0.0, side, side), RectStyle { fill: BACK_COLOR, stroke: DIAG_COLOR, radius: 0.0, stroke_width: 1.0 }, None);
        always(&mut b, k);
    }
    for (i, &(s, e)) in d.blocks.iter().enumerate() {
        let (x0, x1) = (s as f32 * c, e as f32 * c);
        let k = b.add_rect(matrix_id(ID_BLOCK, i as u32), 1, WorldRect::new(x0, x0, x1, x1), RectStyle { fill: BLOCK_COLOR, stroke: ABOVE_COLOR, radius: 0.0, stroke_width: 1.0 }, None);
        always(&mut b, k);
    }
    let max_w = d.cells.iter().map(|x| x.weight(d.params.weight)).max().unwrap_or(0);
    let log_max = (1.0 + max_w as f32).ln().max(f32::MIN_POSITIVE);
    let inset = 0.08 * c;
    for (i, x) in d.cells.iter().enumerate() {
        let t = (1.0 + x.weight(d.params.weight) as f32).ln() / log_max;
        let mut fill = if x.is_feedback() { ABOVE_COLOR } else { BELOW_COLOR };
        fill.0[3] = (90.0 + 165.0 * t).round() as u8;
        let (x0, y0) = (x.col as f32 * c, x.row as f32 * c);
        let k = b.add_rect(matrix_id(ID_CELL, i as u32), 3, WorldRect::new(x0 + inset, y0 + inset, x0 + c - inset, y0 + c - inset), RectStyle { fill, stroke: Rgba::TRANSPARENT, radius: 1.0, stroke_width: 0.0 }, None);
        always(&mut b, k);
    }
    let (mut qid, mut tmp, mut num) = (Vec::new(), Vec::new(), Vec::new());
    for (r, &v) in d.nodes.iter().enumerate() {
        let y = r as f32 * c;
        let k = b.add_rect(matrix_id(ID_ROW, r as u32), 2, WorldRect::new(y, y, y + c, y + c), RectStyle { fill: DIAG_COLOR, stroke: Rgba::TRANSPARENT, radius: 0.0, stroke_width: 0.0 }, None);
        always(&mut b, k);
        // Row header: "123 pkg.mod", alternating shade; external rows muted.
        num.clear();
        push_u32(&mut num, r as u32 + 1);
        qid.clear();
        qid.extend_from_slice(&num);
        qid.push(b' ');
        ix.qid(v, &mut tmp);
        qid.extend_from_slice(&tmp);
        let mut fill = if r % 2 == 0 { Rgba::rgb(48, 52, 60) } else { Rgba::rgb(42, 46, 53) };
        if cm.layout.muted(v) {
            fill = muted(fill);
        }
        let style = RectStyle { fill, stroke: Rgba::TRANSPARENT, radius: 0.0, stroke_width: 0.0 };
        b.add_rect(matrix_id(ID_ROW, r as u32), 0, WorldRect::new(-g.header, y, 0.0, y + c), style, Some(&qid));
        b.add_rect(matrix_id(ID_COL, r as u32), 0, WorldRect::new(y, -g.col_header, y + c, 0.0), style, Some(&num));
    }
    (b.build(), g)
}

/// Appends the decimal digits of `x` (no allocation).
pub fn push_u32(out: &mut Vec<u8>, mut x: u32) {
    let mut d = [0u8; 10];
    let mut k = d.len();
    loop {
        k -= 1;
        d[k] = b'0' + (x % 10) as u8;
        x /= 10;
        if x == 0 {
            break;
        }
    }
    out.extend_from_slice(&d[k..]);
}

/// Marks of node `v` in a code map: its cell outline (whatever band shows it).
pub fn codemap_mark(cm: &CodeMap, v: NodeId) -> Option<RectInstance> {
    let r = cm.layout.rect(v)?;
    (r.w() > 0.0 && r.h() > 0.0).then_some(RectInstance { min: [r.x0, r.y0], max: [r.x1, r.y1], fill: Rgba::TRANSPARENT, stroke: SELECT_COLOR, radius: 2.0, stroke_width: 2.5 })
}
