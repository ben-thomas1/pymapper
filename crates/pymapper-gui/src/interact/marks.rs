//! Persistent overlays ([`crate::map::OVERLAY_MARKS`]): import cycles, contract (layer)
//! violations and finding badges, over the main scene's geometry.
//!
//! ```text
//!   cycles      every member module outlined red (in the bands its cell shows), the cycle's
//!               witness (a -> b -> ... -> a) as red arcs between the modules' cells
//!   layers      importer and imported module outlined orange, an arc between them
//!   badges      a square in the top-right corner of each cell with findings in its subtree,
//!               coloured by the worst severity, while the cell is collapsed; once it opens,
//!               its children carry their own badges and the cell's own findings (module-level)
//!               get a badge in its header band
//! ```
//!
//! Rebuilt only when a toggle, the badge confidence or the scene changes. Cost O(marks).

use pymapper_core::index::NodeId;
use pymapper_core::layout::Rect;
use pymapper_core::rules::{Arg, Severity};

use super::findings::{Badges, Insights};
use super::highlight::{Base, arc, outline};
use crate::codemap::CodeMap;
use crate::map::scene::{LOD_UNBOUNDED, Lod, RectStyle, Rgba, Scene, SceneBuilder, WorldRect};

pub const CYCLE_COLOR: Rgba = Rgba::rgb(255, 70, 70);
pub const LAYER_COLOR: Rgba = Rgba::rgb(255, 150, 20);

/// Badge colour of a severity.
pub fn severity_color(s: Severity) -> Rgba {
    match s {
        Severity::Error => Rgba::rgb(235, 55, 55),
        Severity::Warning => Rgba::rgb(245, 185, 40),
        Severity::Info => Rgba::rgb(90, 160, 255),
    }
}

/// Which persistent overlays are on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MarksSpec {
    pub cycles: bool,
    pub layers: bool,
    pub badges: bool,
    /// Findings below this confidence are not marked (cycles and violations are 100).
    pub min_conf: u8,
}

impl Default for MarksSpec {
    fn default() -> Self {
        MarksSpec { cycles: false, layers: false, badges: false, min_conf: 0 }
    }
}

impl MarksSpec {
    pub fn any(&self) -> bool {
        self.cycles || self.layers || self.badges
    }
}

/// Counts of what was marked.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MarksStats {
    pub cycles: u32,
    pub violations: u32,
    pub badges: u32,
}

/// Builds the persistent overlay scene.
pub fn build(base: &Base<'_>, map: &CodeMap, ins: &Insights, badges: &Badges, spec: &MarksSpec) -> (Scene, MarksStats) {
    let mut b = SceneBuilder::new();
    let mut stats = MarksStats::default();
    if !spec.any() {
        return (Scene::default(), stats);
    }
    if spec.cycles {
        for &i in &ins.cycles {
            let (f, args) = (&ins.findings.as_slice()[i as usize], ins.findings.args(&ins.findings.as_slice()[i as usize]));
            if f.confidence < spec.min_conf {
                continue;
            }
            stats.cycles += 1;
            for a in ins.findings.extra(f) {
                if let Arg::Symbol(s) = *a
                    && let Some(c) = cell_of(base, s.0)
                {
                    let lod = base.scene.rects.lod[c as usize];
                    outline(&mut b, base, c, CYCLE_COLOR, 2.5, lod);
                }
            }
            let witness = args.iter().skip(2).filter_map(|a| if let Arg::Symbol(s) = *a { Some(s.0) } else { None });
            chain(&mut b, base, witness, CYCLE_COLOR, i);
        }
    }
    if spec.layers {
        for lv in &ins.layer_violations {
            let f = &ins.findings.as_slice()[lv.finding as usize];
            if f.confidence < spec.min_conf {
                continue;
            }
            stats.violations += 1;
            for v in [lv.importer, lv.imported] {
                if let Some(c) = cell_of(base, v) {
                    let lod = base.scene.rects.lod[c as usize];
                    outline(&mut b, base, c, LAYER_COLOR, 2.5, lod);
                }
            }
            chain(&mut b, base, [lv.importer, lv.imported].into_iter(), LAYER_COLOR, lv.finding);
        }
    }
    if spec.badges {
        let header = map.layout.config();
        for bd in &badges.list {
            let Some(c) = cell_exact(base, bd.node) else { continue };
            let inst = &base.scene.rects.inst[c as usize];
            let cell_lod = base.scene.rects.lod[c as usize];
            let open = map.reveal.open.get(bd.node as usize).copied().unwrap_or(LOD_UNBOUNDED);
            let r = WorldRect { min: inst.min, max: inst.max };
            // Collapsed: the subtree's findings.
            if cell_lod.min < open {
                stats.badges += 1;
                badge(&mut b, bd.node, &r, bd.subtree, severity_color(bd.worst), Lod { min: cell_lod.min, max: open.min(cell_lod.max) });
            }
            // Open: the node's own findings, in its header band.
            if bd.direct > 0 && open < LOD_UNBOUNDED {
                let h = header.header_band(&Rect::new(r.min[0], r.min[1], r.max[0], r.max[1]));
                if h.h() > 0.0 {
                    stats.badges += 1;
                    badge(&mut b, bd.node, &WorldRect::new(h.x0, h.y0, h.x1, h.y1), bd.direct, severity_color(bd.worst_direct), Lod { min: open.max(cell_lod.min), max: cell_lod.max });
                }
            }
        }
    }
    (b.build(), stats)
}

/// The cell of `v` itself (badges sit on the node they count).
fn cell_exact(base: &Base<'_>, v: NodeId) -> Option<u32> {
    let c = base.sx.cell(v);
    (c != super::sceneindex::NO_CELL).then_some(c)
}

/// The cell of `v`, or of its nearest drawn ancestor.
fn cell_of(base: &Base<'_>, v: NodeId) -> Option<u32> {
    if v >= base.ix.node_count() {
        return None;
    }
    base.cell_or_ancestor(v)
}

/// Arcs between the cells of consecutive nodes.
fn chain(b: &mut SceneBuilder, base: &Base<'_>, nodes: impl Iterator<Item = NodeId>, color: Rgba, id: u32) {
    let mut prev: Option<[f32; 2]> = None;
    for v in nodes {
        let Some(c) = cell_of(base, v) else { continue };
        let inst = &base.scene.rects.inst[c as usize];
        let p = WorldRect { min: inst.min, max: inst.max }.center();
        if let Some(q) = prev
            && q != p
        {
            b.add_curve(id, arc(q, p, 0.2), color, 2.5, Lod::ALWAYS);
        }
        prev = Some(p);
    }
}

/// A badge square in the top-right corner of `r`, larger for more findings.
fn badge(b: &mut SceneBuilder, id: NodeId, r: &WorldRect, count: u32, color: Rgba, lod: Lod) {
    if lod.min >= lod.max {
        return;
    }
    let m = r.width().min(r.height());
    let frac = (0.10 + 0.03 * (count.max(1) as f32).log2()).min(0.30);
    let s = m * frac;
    let pad = m * 0.04;
    let wr = WorldRect::new(r.max[0] - pad - s, r.min[1] + pad, r.max[0] - pad, r.min[1] + pad + s);
    let style = RectStyle { fill: color, stroke: Rgba::new(20, 20, 24, 220), radius: s, stroke_width: 1.0 };
    let i = b.add_rect(id, 3, wr, style, None);
    let auto = b.rect_lod(i);
    b.set_rect_lod(i, Lod { min: lod.min.max(auto.min), max: lod.max });
}
