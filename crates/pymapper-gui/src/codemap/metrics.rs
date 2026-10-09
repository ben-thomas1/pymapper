//! Selectable metrics of the code map: what sizes a cell (the layout's [`SizeMetric`]), what
//! colours it, and what weighs an edge; plus the colour ramps and the legend they produce.

use pymapper_core::index::{Index, NodeId, NodeKind};
use pymapper_core::layout::SizeMetric;
use pymapper_core::rollup::Aggregates;

use crate::map::scene::Rgba;

/// Every size metric, in menu order.
pub const SIZE_METRICS: [SizeMetric; 5] = [SizeMetric::Loc, SizeMetric::Symbols, SizeMetric::CcSum, SizeMetric::CogSum, SizeMetric::Uniform];

/// Menu label of a size metric.
pub fn size_label(m: SizeMetric) -> &'static str {
    match m {
        SizeMetric::Loc => "lines of code",
        SizeMetric::Symbols => "symbols",
        SizeMetric::CcSum => "cyclomatic (sum)",
        SizeMetric::CogSum => "cognitive (sum)",
        SizeMetric::Uniform => "uniform",
    }
}

/// What colours a cell.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum ColorMetric {
    /// Categorical by node kind (distribution, package, module, class, function).
    #[default]
    Kind,
    /// Largest cyclomatic complexity of a function in the subtree (absolute scale).
    CcMax,
    /// Largest cognitive complexity of a function in the subtree (absolute scale).
    CogMax,
    /// Cyclomatic complexity per 100 lines (density; absolute scale).
    CcDensity,
    /// Lines of code (log scale, relative to the largest node of the same depth).
    Loc,
    /// Classes and functions in the subtree (log scale, relative per depth).
    Symbols,
}

impl ColorMetric {
    pub const ALL: [ColorMetric; 6] =
        [ColorMetric::Kind, ColorMetric::CcMax, ColorMetric::CogMax, ColorMetric::CcDensity, ColorMetric::Loc, ColorMetric::Symbols];

    pub fn label(self) -> &'static str {
        match self {
            ColorMetric::Kind => "kind",
            ColorMetric::CcMax => "cyclomatic (max)",
            ColorMetric::CogMax => "cognitive (max)",
            ColorMetric::CcDensity => "cyclomatic per 100 lines",
            ColorMetric::Loc => "lines of code",
            ColorMetric::Symbols => "symbols",
        }
    }
}

/// What weighs an edge `A -> B` (everything from `A`'s code into `B`'s subtree).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum EdgeMetric {
    /// Distinct call sites in `A` with an edge into `B`.
    #[default]
    Sites,
    /// Distinct (caller, callee) symbol pairs.
    Pairs,
    /// Lines of the distinct callees in `B` (each callee counted once, at its full span).
    LocReached,
    /// Distinct callees in `B`: how much of `B`'s API `A` uses.
    ApiSurface,
    /// Call sites weighted by the callee's cyclomatic complexity (`sites x max(1, cc)`).
    Complexity,
    /// Module import rows (the import graph instead of the call graph).
    Imports,
}

/// Number of edge metrics (columns of [`EdgeWeights`]).
pub const EDGE_METRICS: usize = 6;

impl EdgeMetric {
    pub const ALL: [EdgeMetric; EDGE_METRICS] =
        [EdgeMetric::Sites, EdgeMetric::Pairs, EdgeMetric::LocReached, EdgeMetric::ApiSurface, EdgeMetric::Complexity, EdgeMetric::Imports];

    pub fn label(self) -> &'static str {
        match self {
            EdgeMetric::Sites => "call sites",
            EdgeMetric::Pairs => "distinct pairs",
            EdgeMetric::LocReached => "LOC reached",
            EdgeMetric::ApiSurface => "API surface used",
            EdgeMetric::Complexity => "complexity-weighted",
            EdgeMetric::Imports => "imports",
        }
    }

    /// Unit for the legend.
    pub fn unit(self) -> &'static str {
        match self {
            EdgeMetric::Sites => "sites",
            EdgeMetric::Pairs => "pairs",
            EdgeMetric::LocReached => "lines",
            EdgeMetric::ApiSurface => "callees",
            EdgeMetric::Complexity => "cc x sites",
            EdgeMetric::Imports => "imports",
        }
    }

    #[inline]
    pub fn column(self) -> usize {
        self as usize
    }
}

/// The weights of one edge under every [`EdgeMetric`], indexed by [`EdgeMetric::column`].
pub type EdgeWeights = [u32; EDGE_METRICS];

/// Sequential ramp stops (gamma space): dark blue, teal, green, yellow, orange, red.
const RAMP: [[u8; 3]; 6] = [[38, 70, 120], [30, 128, 140], [80, 165, 95], [215, 200, 70], [235, 135, 50], [205, 50, 50]];

/// Colour of `t` in 0..=1 on the sequential ramp (clamped; NaN is the low end).
pub fn ramp(t: f32) -> Rgba {
    let t = if t.is_nan() { 0.0 } else { t.clamp(0.0, 1.0) };
    let x = t * (RAMP.len() - 1) as f32;
    let i = (x as usize).min(RAMP.len() - 2);
    let f = x - i as f32;
    let (a, b) = (RAMP[i], RAMP[i + 1]);
    let mix = |k: usize| (f32::from(a[k]) + (f32::from(b[k]) - f32::from(a[k])) * f).round() as u8;
    Rgba::rgb(mix(0), mix(1), mix(2))
}

/// Base colour of each node kind (by `NodeKind as usize`).
pub const KIND_COLORS: [Rgba; 5] = [
    Rgba::rgb(52, 58, 70),    // distribution
    Rgba::rgb(58, 84, 120),   // package
    Rgba::rgb(64, 118, 128),  // module
    Rgba::rgb(150, 112, 70),  // class
    Rgba::rgb(110, 140, 80),  // function
];

/// Values at which the absolute complexity scales reach the top of the ramp.
const CC_TOP: f32 = 30.0;
const COG_TOP: f32 = 45.0;
const DENSITY_TOP: f32 = 25.0;

/// Per-node colour values for one [`ColorMetric`]: precomputed normalisers, so colouring a
/// node is O(1).
#[derive(Clone, Debug)]
pub struct Colorizer {
    metric: ColorMetric,
    /// ln(1 + max) per depth, for the relative scales.
    log_max: Vec<f32>,
}

impl Colorizer {
    /// Normalisers for `metric` over every node of `ix`. O(nodes) for the relative scales.
    pub fn new(metric: ColorMetric, ix: &Index, agg: &Aggregates) -> Colorizer {
        let mut log_max = Vec::new();
        if matches!(metric, ColorMetric::Loc | ColorMetric::Symbols) {
            let col = if metric == ColorMetric::Loc { agg.loc() } else { agg.symbols() };
            let mut max: Vec<u32> = Vec::new();
            for v in 0..ix.node_count() {
                let d = ix.depth(v) as usize;
                if max.len() <= d {
                    max.resize(d + 1, 0);
                }
                max[d] = max[d].max(col[v as usize]);
            }
            log_max = max.iter().map(|&m| (1.0 + m as f32).ln()).collect();
        }
        Colorizer { metric, log_max }
    }

    pub fn metric(&self) -> ColorMetric {
        self.metric
    }

    /// Position of `v` on the ramp (0..=1), or `None` for the categorical metric.
    pub fn value(&self, ix: &Index, agg: &Aggregates, v: NodeId) -> Option<f32> {
        let i = v as usize;
        let rel = |x: u32| {
            let m = self.log_max.get(ix.depth(v) as usize).copied().unwrap_or(0.0);
            if m > 0.0 { (1.0 + x as f32).ln() / m } else { 0.0 }
        };
        let abs = |x: f32, top: f32| (1.0 + x).ln() / (1.0 + top).ln();
        Some(match self.metric {
            ColorMetric::Kind => return None,
            ColorMetric::CcMax => abs(f32::from(agg.cc_max()[i]), CC_TOP),
            ColorMetric::CogMax => abs(f32::from(agg.cog_max()[i]), COG_TOP),
            ColorMetric::CcDensity => {
                let loc = agg.loc()[i].max(1) as f32;
                abs(agg.cc_sum()[i] as f32 * 100.0 / loc.max(10.0), DENSITY_TOP)
            }
            ColorMetric::Loc => rel(agg.loc()[i]),
            ColorMetric::Symbols => rel(agg.symbols()[i]),
        })
    }

    /// Fill colour of `v` (before muting).
    pub fn color(&self, ix: &Index, agg: &Aggregates, v: NodeId) -> Rgba {
        match self.value(ix, agg, v) {
            Some(t) => ramp(t),
            None => KIND_COLORS[ix.kind(v) as usize],
        }
    }

    /// The legend of this colouring.
    pub fn legend(&self) -> ColorLegend {
        let (lo, hi): (&'static str, String) = match self.metric {
            ColorMetric::Kind => return ColorLegend::Kinds,
            ColorMetric::CcMax => ("1", format!("{CC_TOP}+")),
            ColorMetric::CogMax => ("0", format!("{COG_TOP}+")),
            ColorMetric::CcDensity => ("0", format!("{DENSITY_TOP}+ per 100 lines")),
            ColorMetric::Loc | ColorMetric::Symbols => ("0", "largest at its depth (log)".to_owned()),
        };
        ColorLegend::Ramp { lo, hi }
    }
}

/// How the legend explains cell colours.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ColorLegend {
    /// One swatch per node kind.
    Kinds,
    /// The sequential ramp from `lo` to `hi`.
    Ramp { lo: &'static str, hi: String },
}

/// Display name of a node kind.
pub fn kind_label(k: NodeKind) -> &'static str {
    match k {
        NodeKind::Distribution => "distribution",
        NodeKind::Package => "package",
        NodeKind::Module => "module",
        NodeKind::Class => "class",
        NodeKind::Function => "function",
    }
}

/// `c` blended toward the muted grey (third-party and stdlib cells): 35% colour.
pub fn muted(c: Rgba) -> Rgba {
    let [r, g, b, a] = c.0;
    let m = |x: u8, grey: u8| ((u16::from(x) * 35 + u16::from(grey) * 65) / 100) as u8;
    Rgba::new(m(r, 70), m(g, 72), m(b, 78), a)
}

/// `c` scaled toward black by `f` (0..=1 keeps 1-f of the colour).
pub fn darken(c: Rgba, f: f32) -> Rgba {
    let [r, g, b, a] = c.0;
    let k = (1.0 - f).clamp(0.0, 1.0);
    let s = |x: u8| (f32::from(x) * k).round() as u8;
    Rgba::new(s(r), s(g), s(b), a)
}

/// Linear blend of two colours (gamma space), `t` in 0..=1.
pub fn mix(a: Rgba, b: Rgba, t: f32) -> Rgba {
    let t = t.clamp(0.0, 1.0);
    let m = |x: u8, y: u8| (f32::from(x) + (f32::from(y) - f32::from(x)) * t).round() as u8;
    Rgba::new(m(a.0[0], b.0[0]), m(a.0[1], b.0[1]), m(a.0[2], b.0[2]), m(a.0[3], b.0[3]))
}
