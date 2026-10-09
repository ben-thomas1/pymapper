//! Complexity hotspot analysis (design: Goals, Metric definitions; rules PM101-PM107).
//!
//! ```text
//!   Index (facts defs + MetricsTable, computed during extraction)
//!     -> one pass over the def rows of every file, in FileId order
//!          per function: McCabe, cognitive, nesting, LOC, parameters, MI vs thresholds
//!                        -> PM101..PM106 findings (first-party .py files only)
//!          per file:     aggregates (ModuleHotspots row) -> PM107 on the file's lines
//!     -> HotspotReport: per-file aggregates (struct of arrays) + counters
//! ```
//!
//! # Cost class: O(n) (`CostClass::Linear`)
//!
//! Every metric was computed by the extraction pass ([`crate::extract::metrics`]); this pass is
//! a sequential scan over the def rows and their metrics rows (a few column reads per def, two
//! logarithms for the maintainability index) plus a per-file scratch of one byte per def
//! (reused). No allocation per def, no message formatting (findings hold typed arguments).
//! Memory: [`ModuleHotspots`] holds 33 bytes per file. See "Hotspots (S4.3a)" in
//! `docs/benchmarks.md` for measured costs.
//!
//! # What is reported
//!
//! - Functions (methods and nested functions included, each with its own score) of first-party
//!   `.py` files. Stub files (`.pyi`) and `@overload` signatures are skipped: their bodies are
//!   `...` and their parameters repeat the implementation's.
//! - Lengths exclude docstrings: PM104 checks a function's inclusive line span minus the lines
//!   of the docstrings inside it (its own and those of nested definitions), PM107 the module's
//!   lines minus every docstring of the file (module, classes, functions). Blank and
//!   comment-only lines still count. See "Docstrings" in [`crate::extract::metrics`].
//! - Test code (see [`testfiles`]) is skipped unless `analysis.include-tests-in-metrics`.
//! - Parameters exclude the leading `self` / `cls` of a method (a function defined directly in a
//!   class body that is not a `@staticmethod`, when its first parameter is positional).
//! - Function findings are at the function's name; PM107 is at line 1, column 1 of the module.
//! - Every value is a measured fact: confidence 100.
//!
//! # Aggregates per module
//!
//! [`ModuleHotspots`] has one row per file (also third-party and stub files, for scorecards and
//! the GUI): function and class counts, McCabe and cognitive sums and maxima, maximum nesting,
//! minimum maintainability index, the number of functions over any function threshold
//! (whether or not its rule is enabled) and the function with the highest cognitive
//! complexity. Sums count only functions not nested in another function (a nested function's
//! score is already part of its enclosing function's, see [`crate::extract::metrics`]), as
//! [`crate::rollup::Aggregates`] does; maxima and counts include every function.

use std::cmp::Reverse;
use std::collections::BinaryHeap;

use crate::config::Thresholds;
use crate::extract::facts::{DefKind, def_flags, param_kind};
use crate::extract::events::ParamKind;
use crate::analysis::testfiles;
use crate::index::{FileId, Index, NONE, NodeId};
use crate::roots::Origin;
use crate::rules::codes::{
    COGNITIVE_COMPLEXITY, COMPLEX_FUNCTION, DEEP_NESTING, LONG_FUNCTION, LONG_MODULE, LOW_MAINTAINABILITY,
    TOO_MANY_PARAMETERS,
};
use crate::rules::{Arg, Findings, Location, Pass, Rule, RuleEngine, SymbolId};

#[cfg(test)]
mod tests;

/// Per-file aggregates, struct of arrays indexed by [`FileId`] (see the module docs).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ModuleHotspots {
    functions: Vec<u32>,
    classes: Vec<u32>,
    cc_sum: Vec<u32>,
    cog_sum: Vec<u32>,
    cc_max: Vec<u16>,
    cog_max: Vec<u16>,
    nesting_max: Vec<u8>,
    mi_min: Vec<f32>,
    flagged: Vec<u32>,
    worst: Vec<NodeId>,
}

/// One row of [`ModuleHotspots`] (a copy).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ModuleHotspot {
    /// Functions of the file (methods and nested functions included).
    pub functions: u32,
    /// Classes of the file.
    pub classes: u32,
    /// McCabe complexity summed over the functions not nested in a function.
    pub cc_sum: u32,
    /// Highest McCabe complexity of a function.
    pub cc_max: u16,
    /// Cognitive complexity summed over the functions not nested in a function.
    pub cog_sum: u32,
    /// Highest cognitive complexity of a function.
    pub cog_max: u16,
    /// Deepest block nesting of a function.
    pub nesting_max: u8,
    /// Lowest maintainability index of a function (100 without functions).
    pub mi_min: f32,
    /// Functions over at least one of `max-mccabe`, `max-cognitive`, `max-nesting`,
    /// `max-function-loc` and `max-params`.
    pub flagged: u32,
    /// The function with the highest cognitive complexity (first in source order on ties);
    /// `None` without functions.
    pub worst: Option<NodeId>,
}

impl ModuleHotspots {
    fn with_capacity(n: usize) -> ModuleHotspots {
        ModuleHotspots {
            functions: Vec::with_capacity(n),
            classes: Vec::with_capacity(n),
            cc_sum: Vec::with_capacity(n),
            cog_sum: Vec::with_capacity(n),
            cc_max: Vec::with_capacity(n),
            cog_max: Vec::with_capacity(n),
            nesting_max: Vec::with_capacity(n),
            mi_min: Vec::with_capacity(n),
            flagged: Vec::with_capacity(n),
            worst: Vec::with_capacity(n),
        }
    }

    fn push(&mut self, r: &ModuleHotspot) {
        self.functions.push(r.functions);
        self.classes.push(r.classes);
        self.cc_sum.push(r.cc_sum);
        self.cog_sum.push(r.cog_sum);
        self.cc_max.push(r.cc_max);
        self.cog_max.push(r.cog_max);
        self.nesting_max.push(r.nesting_max);
        self.mi_min.push(r.mi_min);
        self.flagged.push(r.flagged);
        self.worst.push(r.worst.unwrap_or(NONE));
    }

    /// Number of files.
    #[inline]
    pub fn len(&self) -> usize {
        self.functions.len()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.functions.is_empty()
    }

    /// The row of file `f`.
    pub fn get(&self, f: FileId) -> ModuleHotspot {
        let i = f as usize;
        let w = self.worst[i];
        ModuleHotspot {
            functions: self.functions[i],
            classes: self.classes[i],
            cc_sum: self.cc_sum[i],
            cc_max: self.cc_max[i],
            cog_sum: self.cog_sum[i],
            cog_max: self.cog_max[i],
            nesting_max: self.nesting_max[i],
            mi_min: self.mi_min[i],
            flagged: self.flagged[i],
            worst: (w != NONE).then_some(w),
        }
    }

    /// Functions over a function threshold, per file.
    #[inline]
    pub fn flagged(&self) -> &[u32] {
        &self.flagged
    }

    /// Summed McCabe complexity per file.
    #[inline]
    pub fn cc_sum(&self) -> &[u32] {
        &self.cc_sum
    }

    /// Summed cognitive complexity per file.
    #[inline]
    pub fn cog_sum(&self) -> &[u32] {
        &self.cog_sum
    }

    /// Heap bytes (capacity).
    pub fn heap_bytes(&self) -> usize {
        4 * (self.functions.capacity()
            + self.classes.capacity()
            + self.cc_sum.capacity()
            + self.cog_sum.capacity()
            + self.mi_min.capacity()
            + self.flagged.capacity()
            + self.worst.capacity())
            + 2 * (self.cc_max.capacity() + self.cog_max.capacity())
            + self.nesting_max.capacity()
    }
}

/// Counters of one run.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HotspotStats {
    /// Files scanned.
    pub files: u64,
    /// Functions scanned (`@overload` signatures excluded).
    pub functions: u64,
    /// Classes scanned.
    pub classes: u64,
    /// Functions over at least one function threshold (all files).
    pub flagged: u64,
    /// Findings pushed.
    pub findings: u64,
}

/// Result of [`analyse`].
#[derive(Clone, Debug, Default, PartialEq)]
pub struct HotspotReport {
    pub modules: ModuleHotspots,
    pub stats: HotspotStats,
}

/// Which rules emit, resolved once per run (one bit test per def otherwise).
#[derive(Clone, Copy)]
struct Emit {
    mccabe: bool,
    cognitive: bool,
    nesting: bool,
    loc: bool,
    params: bool,
    mi: bool,
    module_loc: bool,
}

impl Emit {
    fn of(engine: &RuleEngine) -> Emit {
        let on = |r: Rule| engine.is_enabled(r);
        Emit {
            mccabe: on(COMPLEX_FUNCTION),
            cognitive: on(COGNITIVE_COMPLEXITY),
            nesting: on(DEEP_NESTING),
            loc: on(LONG_FUNCTION),
            params: on(TOO_MANY_PARAMETERS),
            mi: on(LOW_MAINTAINABILITY),
            module_loc: on(LONG_MODULE),
        }
    }

    fn any(&self) -> bool {
        self.mccabe || self.cognitive || self.nesting || self.loc || self.params || self.mi || self.module_loc
    }

    const NONE: Emit =
        Emit { mccabe: false, cognitive: false, nesting: false, loc: false, params: false, mi: false, module_loc: false };
}

/// Whether a file is a stub (`.pyi`).
fn is_stub(ix: &Index, f: FileId) -> bool {
    ix.file_path(f).as_os_str().as_encoded_bytes().ends_with(b".pyi")
}

/// Runs the hotspot pass over every file of `ix`: appends the findings of the enabled PM1xx
/// rules to `out` (in FileId, then source order) and returns the per-file aggregates, which
/// are built even when no rule is enabled (they cost the same scan).
pub fn analyse(ix: &Index, engine: &RuleEngine, out: &mut Findings) -> HotspotReport {
    let emit = if engine.should_run(Pass::Hotspot) { Emit::of(engine) } else { Emit::NONE };
    let excluded = if emit.any() { testfiles::excluded_files(ix, engine.include_tests_in_metrics()) } else { Vec::new() };
    scan(ix, engine.thresholds(), emit, &excluded, out)
}

/// The per-file aggregates only (no findings), with the given thresholds for
/// [`ModuleHotspot::flagged`].
pub fn aggregates(ix: &Index, thresholds: &Thresholds) -> HotspotReport {
    scan(ix, thresholds, Emit::NONE, &[], &mut Findings::new())
}

/// A per-function metric to rank by ([`top_functions`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Metric {
    Mccabe,
    Cognitive,
    Nesting,
    Loc,
    /// Parameters without the leading `self` / `cls` of a method.
    Params,
    /// Lowest maintainability index first.
    Maintainability,
}

/// The `k` worst functions by `metric` among the files for which `include` returns true
/// (`@overload` signatures excluded), worst first; ties go to the function first in node
/// order. Values are the metric (maintainability: the index times 100, truncated). O(defs log
/// k) time, O(k) memory.
pub fn top_functions(ix: &Index, metric: Metric, k: usize, include: impl Fn(FileId) -> bool) -> Vec<(NodeId, u32)> {
    if k == 0 {
        return Vec::new();
    }
    let defs = &ix.facts().defs;
    let mx = ix.metrics();
    // Min-heap of the best `k` so far by (score, earlier node first): the root is the weakest.
    let mut heap: BinaryHeap<Reverse<(u32, Reverse<NodeId>)>> = BinaryHeap::with_capacity(k + 1);
    for f in 0..ix.file_count() {
        let node = ix.file_node(f);
        if node == NONE || !include(f) {
            continue;
        }
        let ff = ix.file_facts(f);
        let base = ff.defs.start as usize;
        for d in 1..ff.defs.len as usize {
            let g = base + d;
            let flags = defs.flags[g];
            if defs.kind[g] != DefKind::Function as u8 || flags & def_flags::OVERLOAD != 0 {
                continue;
            }
            let m = base - f as usize + d - 1;
            let value = match metric {
                Metric::Mccabe => u32::from(mx.cyclomatic[m]),
                Metric::Cognitive => u32::from(mx.cognitive[m]),
                Metric::Nesting => u32::from(mx.max_nesting[m]),
                Metric::Loc => mx.code_lines(m),
                Metric::Params => effective_params(ix, f, g, flags),
                // Lower is worse: rank by 10000 - 100 * MI.
                Metric::Maintainability => 10_000 - (mx.mi(m) * 100.0) as u32,
            };
            let item = Reverse((value, Reverse(node + d as u32)));
            if heap.len() < k {
                heap.push(item);
            } else if let Some(mut weakest) = heap.peek_mut()
                && item < *weakest
            {
                *weakest = item;
            }
        }
    }
    let mut out: Vec<(NodeId, u32)> = Vec::with_capacity(heap.len());
    for Reverse((value, Reverse(v))) in heap.into_sorted_vec() {
        let value = if metric == Metric::Maintainability { 10_000 - value } else { value };
        out.push((v, value));
    }
    out
}

/// Parameters of function row `g` of file `f`, without the leading `self` / `cls` of a method.
#[inline]
fn effective_params(ix: &Index, f: FileId, g: usize, flags: u16) -> u32 {
    let defs = &ix.facts().defs;
    let np = u32::from(defs.params_len[g]);
    if np == 0 || flags & def_flags::METHOD == 0 || flags & def_flags::STATICMETHOD != 0 {
        return np;
    }
    let first = (ix.file_facts(f).params.start + defs.params[g]) as usize;
    match param_kind(ix.facts().params.flags[first]) {
        ParamKind::PositionalOnly | ParamKind::Normal => np - 1,
        _ => np,
    }
}

/// `excluded`: files whose findings are skipped (test code, see [`testfiles`]); aggregates
/// still cover them.
/// Lines of a module without its docstrings (module docstring and every definition's), at
/// least 1 for a non-empty file: the length PM107 checks.
#[inline]
pub fn module_code_lines(lines: u32, doc_lines: u32) -> u32 {
    if lines == 0 { 0 } else { lines.saturating_sub(doc_lines).max(1) }
}

fn scan(ix: &Index, t: &Thresholds, emit: Emit, excluded: &[bool], out: &mut Findings) -> HotspotReport {
    let n = ix.file_count() as usize;
    let defs = &ix.facts().defs;
    let mx = ix.metrics();
    let mut modules = ModuleHotspots::with_capacity(n);
    let mut stats = HotspotStats { files: n as u64, ..HotspotStats::default() };
    let before = out.len();
    // Per def of the current file: inside a function (reused across files).
    let mut in_fn: Vec<bool> = Vec::new();
    for f in 0..n as FileId {
        let ff = ix.file_facts(f);
        let node = ix.file_node(f);
        let report = node != NONE
            && ix.file_origin(f) == Origin::FirstParty
            && !is_stub(ix, f)
            && !testfiles::is_excluded(excluded, f);
        let file = crate::rules::FileId(f);
        let mut row = ModuleHotspot {
            functions: 0,
            classes: 0,
            cc_sum: 0,
            cc_max: 0,
            cog_sum: 0,
            cog_max: 0,
            nesting_max: 0,
            mi_min: 100.0,
            flagged: 0,
            worst: None,
        };
        let len = ff.defs.len as usize;
        let base = ff.defs.start as usize;
        in_fn.clear();
        in_fn.resize(len, false);
        for d in 1..len {
            let g = base + d;
            let kind = defs.kind[g];
            // Parents come before children (pre-order); row 0 is `<module>`.
            let p = defs.parent[g] as usize;
            let nested = p < d && p > 0 && (in_fn[p] || defs.kind[base + p] == DefKind::Function as u8);
            in_fn[d] = nested;
            if kind == DefKind::Class as u8 {
                row.classes += 1;
                continue;
            }
            let flags = defs.flags[g];
            if kind != DefKind::Function as u8 || flags & def_flags::OVERLOAD != 0 {
                continue;
            }
            // One metrics row per def row except `<module>`, files in FileId order.
            let m = base - f as usize + d - 1;
            let cc = mx.cyclomatic[m];
            let cog = mx.cognitive[m];
            let nest = mx.max_nesting[m];
            // PM104 and `flagged` measure lines without docstrings; MI keeps the full span.
            let loc = mx.code_lines(m);
            let mi = mx.mi(m);
            let np = effective_params(ix, f, g, flags);
            let v = node.wrapping_add(d as u32);
            row.functions += 1;
            if !nested {
                row.cc_sum = row.cc_sum.saturating_add(u32::from(cc));
                row.cog_sum = row.cog_sum.saturating_add(u32::from(cog));
            }
            row.cc_max = row.cc_max.max(cc);
            if row.functions == 1 || cog > row.cog_max {
                row.worst = (node != NONE).then_some(v);
            }
            row.cog_max = row.cog_max.max(cog);
            row.nesting_max = row.nesting_max.max(nest);
            row.mi_min = row.mi_min.min(mi as f32);
            let over_cc = u32::from(cc) > t.max_mccabe;
            let over_cog = u32::from(cog) > t.max_cognitive;
            let over_nest = u32::from(nest) > t.max_nesting;
            let over_loc = loc > t.max_function_loc;
            let over_params = np > t.max_params;
            if over_cc || over_cog || over_nest || over_loc || over_params {
                row.flagged += 1;
            }
            if !report {
                continue;
            }
            let sym = SymbolId(v);
            let at = Location::new(file, defs.name_line[g], defs.name_col[g] + 1);
            let a = Arg::Symbol(sym);
            if emit.mccabe && over_cc {
                out.push(COMPLEX_FUNCTION, at, sym, 100, &[a, Arg::U32(cc.into()), Arg::U32(t.max_mccabe)]);
            }
            if emit.cognitive && over_cog {
                out.push(COGNITIVE_COMPLEXITY, at, sym, 100, &[a, Arg::U32(cog.into()), Arg::U32(t.max_cognitive)]);
            }
            if emit.nesting && over_nest {
                out.push(DEEP_NESTING, at, sym, 100, &[a, Arg::U32(nest.into()), Arg::U32(t.max_nesting)]);
            }
            if emit.loc && over_loc {
                out.push(LONG_FUNCTION, at, sym, 100, &[a, Arg::U32(loc), Arg::U32(t.max_function_loc)]);
            }
            if emit.params && over_params {
                out.push(TOO_MANY_PARAMETERS, at, sym, 100, &[a, Arg::U32(np), Arg::U32(t.max_params)]);
            }
            if emit.mi && mi < t.min_maintainability {
                let limit = t.min_maintainability as f32;
                out.push(LOW_MAINTAINABILITY, at, sym, 100, &[a, Arg::F32(mi as f32), Arg::F32(limit)]);
            }
        }
        let module_lines = module_code_lines(ff.lines, ff.doc_lines);
        if emit.module_loc && report && module_lines > t.max_module_loc {
            let sym = SymbolId(node);
            let args = [Arg::Symbol(sym), Arg::U32(module_lines), Arg::U32(t.max_module_loc)];
            out.push_ext(LONG_MODULE, Location::new(file, 1, 1), ff.lines.max(1), sym, 100, &args, &[]);
        }
        stats.functions += u64::from(row.functions);
        stats.classes += u64::from(row.classes);
        stats.flagged += u64::from(row.flagged);
        modules.push(&row);
    }
    stats.findings = (out.len() - before) as u64;
    HotspotReport { modules, stats }
}
