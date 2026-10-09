//! Per-distribution scorecards: size, complexity, hotspots, maintainability, coupling, grade.
//!
//! Everything here depends only on the distribution's own facts and on how its imports
//! resolved, never on the project or the configuration, so a scorecard can be cached with the
//! distribution (see [`super::store`]).
//!
//! # Metrics (non-stub files of the distribution)
//!
//! - Size: `files` (stub files counted apart in `stub_files`), `loc` (lines of the files),
//!   `code_lines` (`loc` without docstrings), `classes`, `functions` (methods and nested
//!   functions included, `@overload` signatures excluded), `public_api` (classes and
//!   functions reachable by a public dotted path: defined with no package, module, class or
//!   function on the path starting with `_`, not nested in a function, dunders excluded; or
//!   re-exported by a public module, see [`res_flags::EXPORTED`]).
//! - Complexity: mean and maximum McCabe and cognitive complexity per function, deepest
//!   nesting.
//! - Hotspots: functions over at least one of the default function thresholds (McCabe 10,
//!   cognitive 15, nesting 4, 60 code lines, 6 parameters; the defaults of `[thresholds]`,
//!   fixed so scorecards compare across projects); `hotspot_density` = hotspots per 1000 code
//!   lines.
//! - Maintainability: mean maintainability index of the functions weighted by their line span
//!   (`mi_mean`), and the lowest (`mi_min`).
//! - Coupling, over module import edges that are not type-only: `internal_edges` (between two
//!   modules of the distribution), `fan_out_mean` (internal edges per module) and
//!   `fan_out_max`, `cyclic_modules` (modules in an import cycle inside the distribution),
//!   `external_deps` (distinct other distributions and unattributed top-level packages
//!   imported; the stdlib and first-party code are not counted).
//!
//! # Grade
//!
//! Four sub-scores in `[0, 1]` (`clamp01(x) = min(max(x, 0), 1)`):
//!
//! ```text
//!   hotspots        H = 1 - clamp01(hotspot_density / 15)
//!   maintainability M = clamp01((mi_mean - 25) / 40)
//!   complexity      C = 1 - clamp01((cc_mean - 1.5) / 5)
//!   coupling        K = 1 - 0.5 * clamp01(2 * cyclic_modules / files)
//!                       - 0.5 * clamp01((fan_out_mean - 1) / 4)
//!   score = 100 * (0.30 H + 0.25 M + 0.25 C + 0.20 K)
//! ```
//!
//! Grades: A >= 85, B >= 70, C >= 55, D >= 40, else F. The ranges were calibrated on a venv of
//! 491 distributions (477 with functions, 16.2M lines; median score 70, grades A 19%, B 32%,
//! C 35%, D 12%, F 2%): the grade says how a distribution compares with typical published
//! Python code. A distribution without functions (stubs only, data, compiled code) has no
//! score and no grade. Size is reported but not graded: a large library is not worse for
//! being large.

use crate::analysis::hotspot::{effective_params, module_code_lines};
use crate::extract::facts::{DefKind, def_flags};
use crate::graph::{Graph, Scc};
use crate::index::{FileId, Index, NONE, NodeId};
use crate::intern::FxHashMap;
use crate::resolve::{ExtKind, Imports, res_flags};
use crate::roots::Origin;

use super::GroupFiles;

/// Bump whenever a metric or the grade formula changes: cached scorecards of other versions
/// are recomputed.
pub const SCORE_VERSION: u32 = 1;

/// Default function thresholds used for hotspots (see the module docs).
const MAX_MCCABE: u16 = 10;
const MAX_COGNITIVE: u16 = 15;
const MAX_NESTING: u8 = 4;
const MAX_FUNCTION_LOC: u32 = 60;
const MAX_PARAMS: u32 = 6;

/// Letter grade of a score.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Grade {
    A,
    B,
    C,
    D,
    F,
}

impl Grade {
    /// Grade of a score in `0..=100` (see the module docs).
    pub fn of(score: f32) -> Grade {
        if score >= 85.0 {
            Grade::A
        } else if score >= 70.0 {
            Grade::B
        } else if score >= 55.0 {
            Grade::C
        } else if score >= 40.0 {
            Grade::D
        } else {
            Grade::F
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Grade::A => "A",
            Grade::B => "B",
            Grade::C => "C",
            Grade::D => "D",
            Grade::F => "F",
        }
    }
}

/// The scorecard of one distribution (see the module docs). 100 bytes, plain data.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Scorecard {
    pub files: u32,
    pub stub_files: u32,
    pub loc: u32,
    pub code_lines: u32,
    pub classes: u32,
    pub functions: u32,
    pub public_api: u32,
    pub cc_mean: f32,
    pub cc_max: u32,
    pub cog_mean: f32,
    pub cog_max: u32,
    pub nesting_max: u32,
    pub hotspots: u32,
    pub hotspot_density: f32,
    pub mi_mean: f32,
    pub mi_min: f32,
    pub internal_edges: u32,
    pub fan_out_mean: f32,
    pub fan_out_max: u32,
    pub cyclic_modules: u32,
    pub external_deps: u32,
    /// 0-100; NaN without functions.
    pub score: f32,
}

/// Number of `u32` words of an encoded [`Scorecard`].
pub(crate) const WORDS: usize = 22;

#[inline]
fn clamp01(x: f32) -> f32 {
    x.clamp(0.0, 1.0)
}

impl Scorecard {
    /// The grade, `None` without functions.
    pub fn grade(&self) -> Option<Grade> {
        (!self.score.is_nan()).then(|| Grade::of(self.score))
    }

    /// The score formula of the module docs over the other fields; NaN without functions.
    pub fn compute_score(&self) -> f32 {
        if self.functions == 0 {
            return f32::NAN;
        }
        let h = 1.0 - clamp01(self.hotspot_density / 15.0);
        let m = clamp01((self.mi_mean - 25.0) / 40.0);
        let c = 1.0 - clamp01((self.cc_mean - 1.5) / 5.0);
        let cyclic = if self.files == 0 { 0.0 } else { self.cyclic_modules as f32 / self.files as f32 };
        let k = 1.0 - 0.5 * clamp01(2.0 * cyclic) - 0.5 * clamp01((self.fan_out_mean - 1.0) / 4.0);
        100.0 * (0.30 * h + 0.25 * m + 0.25 * c + 0.20 * k)
    }

    /// Fixed-layout encoding (reals as their bits) for the store.
    pub(crate) fn to_words(self) -> [u32; WORDS] {
        [
            self.files,
            self.stub_files,
            self.loc,
            self.code_lines,
            self.classes,
            self.functions,
            self.public_api,
            self.cc_mean.to_bits(),
            self.cc_max,
            self.cog_mean.to_bits(),
            self.cog_max,
            self.nesting_max,
            self.hotspots,
            self.hotspot_density.to_bits(),
            self.mi_mean.to_bits(),
            self.mi_min.to_bits(),
            self.internal_edges,
            self.fan_out_mean.to_bits(),
            self.fan_out_max,
            self.cyclic_modules,
            self.external_deps,
            self.score.to_bits(),
        ]
    }

    pub(crate) fn from_words(w: &[u32; WORDS]) -> Scorecard {
        Scorecard {
            files: w[0],
            stub_files: w[1],
            loc: w[2],
            code_lines: w[3],
            classes: w[4],
            functions: w[5],
            public_api: w[6],
            cc_mean: f32::from_bits(w[7]),
            cc_max: w[8],
            cog_mean: f32::from_bits(w[9]),
            cog_max: w[10],
            nesting_max: w[11],
            hotspots: w[12],
            hotspot_density: f32::from_bits(w[13]),
            mi_mean: f32::from_bits(w[14]),
            mi_min: f32::from_bits(w[15]),
            internal_edges: w[16],
            fan_out_mean: f32::from_bits(w[17]),
            fan_out_max: w[18],
            cyclic_modules: w[19],
            external_deps: w[20],
            score: f32::from_bits(w[21]),
        }
    }
}

/// Reused buffers of [`compute`] (one per worker).
#[derive(Default)]
pub struct Scratch {
    /// Per def row of the current file: public (see the module docs).
    public: Vec<bool>,
    /// External dependency keys of the current distribution.
    keys: Vec<u64>,
    /// Internal module edges (local file indices) of the current distribution.
    edges: Vec<(u32, u32)>,
    /// Public API symbols, one bit per node of the distribution's subtree.
    marks: Vec<u64>,
}

impl Scratch {
    /// Marks node `v` (`v - base` must be inside the cleared range).
    #[inline]
    fn mark(&mut self, base: NodeId, v: NodeId) {
        let i = (v - base) as usize;
        self.marks[i / 64] |= 1 << (i % 64);
    }
}

/// What [`compute`] reads besides the group: the index, its imports, the files per group and
/// the group of every distribution node.
pub struct ScoreCtx<'a> {
    pub ix: &'a Index,
    pub imp: &'a Imports,
    pub files: &'a GroupFiles,
    dist_group: FxHashMap<NodeId, u32>,
}

impl<'a> ScoreCtx<'a> {
    pub fn new(ix: &'a Index, imp: &'a Imports, files: &'a GroupFiles) -> ScoreCtx<'a> {
        let mut dist_group = FxHashMap::default();
        for g in 0..ix.groups().len() as u32 {
            dist_group.insert(ix.group_node(g), g);
        }
        ScoreCtx { ix, imp, files, dist_group }
    }
}

/// Whether file `f` is a stub (`.pyi`).
#[inline]
fn is_stub(ix: &Index, f: FileId) -> bool {
    ix.file_path(f).as_os_str().as_encoded_bytes().ends_with(b".pyi")
}

/// Whether a dotted module name has a private component (`pkg._impl.mod`).
fn private_module(name: &[u8]) -> bool {
    name.split(|&b| b == b'.').any(|c| c.first() == Some(&b'_'))
}

/// The scorecard of group `g` (see the module docs). O(defs + import edges) of the group.
pub fn compute(cx: &ScoreCtx<'_>, g: u32, s: &mut Scratch) -> Scorecard {
    let (ix, imp, files) = (cx.ix, cx.imp, cx.files);
    let defs = &ix.facts().defs;
    let mx = ix.metrics();
    let own = files.of(g);
    let own_dist = ix.groups()[g as usize].dist;
    let group_node = ix.group_node(g);
    let mut sc = Scorecard { mi_min: 100.0, ..Scorecard::default() };
    let (mut cc_total, mut cog_total) = (0u64, 0u64);
    let (mut mi_sum, mut mi_weight) = (0f64, 0f64);
    s.keys.clear();
    s.edges.clear();
    let span = ix.subtree(group_node);
    s.marks.clear();
    s.marks.resize((span.end - span.start).div_ceil(64) as usize, 0);
    // Fan-out per local file index, counted as edges are seen.
    let mut fan_max = 0u32;
    for (local, &f) in own.iter().enumerate() {
        if is_stub(ix, f) {
            sc.stub_files += 1;
            continue;
        }
        let ff = ix.file_facts(f);
        sc.files += 1;
        sc.loc = sc.loc.saturating_add(ff.lines);
        sc.code_lines = sc.code_lines.saturating_add(module_code_lines(ff.lines, ff.doc_lines));
        let module_public = !private_module(ix.str(ix.file_module(f)));
        let base = ff.defs.start as usize;
        let len = ff.defs.len as usize;
        s.public.clear();
        s.public.resize(len, false);
        if len > 0 {
            s.public[0] = module_public;
        }
        for d in 1..len {
            let row = base + d;
            let kind = defs.kind[row];
            let flags = defs.flags[row];
            let p = defs.parent[row] as usize;
            let parent_public = p < d && s.public[p];
            let public = parent_public
                && flags & (def_flags::PRIVATE | def_flags::DUNDER | def_flags::NESTED | def_flags::OVERLOAD) == 0;
            s.public[d] = public;
            if public {
                s.mark(group_node, ix.file_node(f) + d as u32);
            }
            if kind == DefKind::Class as u8 {
                sc.classes += 1;
                continue;
            }
            if kind != DefKind::Function as u8 || flags & def_flags::OVERLOAD != 0 {
                continue;
            }
            // One metrics row per def row except `<module>`, files in FileId order.
            let m = base - f as usize + d - 1;
            let cc = mx.cyclomatic[m];
            let cog = mx.cognitive[m];
            let nest = mx.max_nesting[m];
            let mi = mx.mi(m);
            let span = f64::from(mx.loc[m].max(1));
            sc.functions += 1;
            cc_total += u64::from(cc);
            cog_total += u64::from(cog);
            sc.cc_max = sc.cc_max.max(u32::from(cc));
            sc.cog_max = sc.cog_max.max(u32::from(cog));
            sc.nesting_max = sc.nesting_max.max(u32::from(nest));
            mi_sum += mi * span;
            mi_weight += span;
            sc.mi_min = sc.mi_min.min(mi as f32);
            let hot = cc > MAX_MCCABE
                || cog > MAX_COGNITIVE
                || nest > MAX_NESTING
                || mx.code_lines(m) > MAX_FUNCTION_LOC
                || effective_params(ix, f, row, flags) > MAX_PARAMS;
            sc.hotspots += u32::from(hot);
        }
        // Re-exports of a public module are public API wherever they are defined.
        if module_public {
            for row in ff.imports.range() {
                if imp.flags(row) & res_flags::EXPORTED == 0 {
                    continue;
                }
                if let Some(t) = imp.target(row)
                    && ix.kind(t).is_def()
                    && span.contains(&t)
                {
                    s.mark(group_node, t);
                }
            }
        }
        // Coupling: module edges of the file.
        let mut fan = 0u32;
        for e in imp.module_edges(f) {
            if e.flags & res_flags::TYPE_ONLY != 0 {
                continue;
            }
            if ix.is_descendant(group_node, e.dst) {
                fan += 1;
                if let Some(df) = ix.file(e.dst)
                    && let Ok(dl) = own.binary_search(&df)
                {
                    s.edges.push((local as u32, dl as u32));
                }
                continue;
            }
            let dg = match ix.file(e.dst) {
                Some(df) => ix.file_group(df),
                None => match cx.dist_group.get(&ix.ancestor_at(e.dst, 0)) {
                    Some(&dg) => dg,
                    None => continue,
                },
            };
            let other = &ix.groups()[dg as usize];
            match (other.origin, other.dist) {
                (Origin::ThirdParty, Some(d)) if Some(d) != own_dist => s.keys.push((1 << 32) | u64::from(d.0)),
                (Origin::ThirdParty, None) => s.keys.push((3 << 32) | u64::from(dg)),
                _ => {}
            }
        }
        for e in imp.ext_edges(f) {
            if e.flags & res_flags::TYPE_ONLY != 0 {
                continue;
            }
            match e.kind {
                ExtKind::ThirdParty if own_dist.is_none_or(|d| d.0 != e.key) => s.keys.push((1 << 32) | u64::from(e.key)),
                ExtKind::Unknown if e.key != NONE => s.keys.push((2 << 32) | u64::from(e.key)),
                _ => {}
            }
        }
        sc.internal_edges += fan;
        fan_max = fan_max.max(fan);
    }
    sc.fan_out_max = fan_max;
    sc.public_api = s.marks.iter().map(|w| w.count_ones()).sum();
    if sc.files > 0 {
        sc.fan_out_mean = sc.internal_edges as f32 / sc.files as f32;
    }
    if sc.functions > 0 {
        sc.cc_mean = (cc_total as f64 / f64::from(sc.functions)) as f32;
        sc.cog_mean = (cog_total as f64 / f64::from(sc.functions)) as f32;
        sc.mi_mean = (mi_sum / mi_weight.max(1.0)) as f32;
    } else {
        sc.mi_min = 100.0;
    }
    if sc.code_lines > 0 {
        sc.hotspot_density = (f64::from(sc.hotspots) * 1000.0 / f64::from(sc.code_lines)) as f32;
    }
    s.keys.sort_unstable();
    s.keys.dedup();
    sc.external_deps = s.keys.len() as u32;
    sc.cyclic_modules = cyclic_modules(own.len() as u32, &s.edges);
    sc.score = sc.compute_score();
    sc
}

/// Modules in a strongly connected component of more than one module.
fn cyclic_modules(n: u32, edges: &[(u32, u32)]) -> u32 {
    if edges.len() < 2 {
        return 0;
    }
    let g = Graph::from_edges(n, edges);
    let scc = Scc::of(&g);
    scc.nontrivial().map(|c| scc.size(c)).sum()
}
