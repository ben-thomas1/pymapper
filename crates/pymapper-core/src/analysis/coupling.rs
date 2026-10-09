//! Coupling analysis (design: Goals; rules PM201-PM204).
//!
//! ```text
//!   Index + Imports (module edges per file) ──> per file: Ce (distinct imported modules),
//!                                               Ca (distinct importing modules), classes,
//!                                               abstract classes, symbols
//!                                          ──> instability, abstractness, distance from the
//!                                               main sequence (computed on demand)
//!                                          ──> PM201 high-fan-out, PM202 main-sequence-distance,
//!                                               PM203 god-module
//!   + call Graph over nodes (optional)     ──> per class above `max-class-wmc`: distinct other
//!                                               classes called -> PM204 god-class
//!   call Graph                             ──> fan_in / fan_out of any node (degrees)
//! ```
//!
//! # Cost class: O(V+E) (`CostClass::Graph`)
//!
//! - Module metrics: one pass over the module edges of [`Imports`] (already aggregated per
//!   (importing module, imported module), so distinct by construction) and one pass over the
//!   def rows and base rows of every file for abstractness. No graph is built: everything is
//!   indexed by [`FileId`]. Memory: [`ModuleCoupling`] holds 20 bytes per file, plus a per-file
//!   scratch of one byte per def (reused).
//! - God classes: only classes whose WMC exceeds `max-class-wmc` (few) look at the call graph:
//!   the successors of every node of the class's subtree, each mapped to its nearest enclosing
//!   class by walking parents (O(depth)), then sorted and deduplicated in a reused buffer. So
//!   the cost is O(calls of the candidate classes), not of the program.
//! - Function fan-in / fan-out are the degrees of the call graph ([`fan_in`], [`fan_out`]):
//!   O(1) per query (plus a binary search for a self loop), nothing stored.
//!
//! See "Coupling (S4.3a)" in `docs/benchmarks.md` for measured costs.
//!
//! # Definitions
//!
//! - An import edge counts when its flags are not type-only (`if TYPE_CHECKING:` imports are
//!   excluded) and both ends are package/module files (namespace packages have no code) in the
//!   same scope: both first-party, or both in the same distribution. Stub files (`.pyi`) are
//!   not sources (their imports repeat the implementation's).
//! - Ce (efferent coupling, the module's fan-out): distinct modules the module imports. Ca
//!   (afferent coupling, fan-in): distinct modules importing it.
//! - Instability `I = Ce / (Ca + Ce)` (undefined, `None`, for an uncoupled module).
//!   Abstractness `A = abstract classes / classes` (0 without classes). A class is abstract
//!   when a base's last name is `ABC` or `Protocol` (`abc.ABC`, `typing.Protocol[T]`, ...), its
//!   `metaclass=` keyword's last name is `ABCMeta`, or one of its methods is an
//!   `@abstractmethod`. Distance from the main sequence `D = |A + I - 1|`.
//! - Symbols: the classes and functions of the file (methods and nested functions included).
//! - Class WMC: the McCabe value of the class row (its methods' scores plus class-level
//!   statements, see [`crate::extract::metrics`]). Class fan-out: distinct classes, other than
//!   the class and the classes nested in it, that contain the target of a call edge from the
//!   class's subtree (a constructor edge targets the class itself; a method targets its class).
//!   Calls to plain functions do not count. Which edges count is up to the caller's
//!   [`CallFilter`](crate::graph::CallFilter) when building the graph.
//! - Findings are for first-party `.py` files only, at line 1, column 1 of the module, and
//!   for a god class at its name. Every value is a measured fact: confidence 100.

use crate::config::Thresholds;
use crate::extract::facts::{DefKind, FileView, def_flags};
use crate::graph::Graph;
use crate::index::{FileId, Index, NONE, NodeId, NodeKind};
use crate::intern::Sym;
use crate::resolve::{Imports, res_flags};
use crate::roots::Origin;
use crate::rules::codes::{GOD_CLASS, GOD_MODULE, HIGH_FAN_OUT, MAIN_SEQUENCE_DISTANCE};
use crate::rules::{Arg, Findings, Location, Pass, RuleEngine, SymbolId};

#[cfg(test)]
mod tests;

/// Per-file coupling counts, struct of arrays indexed by [`FileId`] (see the module docs).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ModuleCoupling {
    ca: Vec<u32>,
    ce: Vec<u32>,
    classes: Vec<u32>,
    abstracts: Vec<u32>,
    symbols: Vec<u32>,
}

impl ModuleCoupling {
    /// Number of files.
    #[inline]
    pub fn len(&self) -> usize {
        self.ca.len()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.ca.is_empty()
    }

    /// Afferent coupling: distinct modules importing file `f`'s module.
    #[inline]
    pub fn ca(&self, f: FileId) -> u32 {
        self.ca[f as usize]
    }

    /// Efferent coupling: distinct modules file `f` imports.
    #[inline]
    pub fn ce(&self, f: FileId) -> u32 {
        self.ce[f as usize]
    }

    /// Afferent coupling of every file.
    #[inline]
    pub fn ca_all(&self) -> &[u32] {
        &self.ca
    }

    /// Efferent coupling of every file.
    #[inline]
    pub fn ce_all(&self) -> &[u32] {
        &self.ce
    }

    /// Classes of file `f`.
    #[inline]
    pub fn classes(&self, f: FileId) -> u32 {
        self.classes[f as usize]
    }

    /// Abstract classes of file `f`.
    #[inline]
    pub fn abstract_classes(&self, f: FileId) -> u32 {
        self.abstracts[f as usize]
    }

    /// Classes and functions of file `f`.
    #[inline]
    pub fn symbols(&self, f: FileId) -> u32 {
        self.symbols[f as usize]
    }

    /// `Ce / (Ca + Ce)`; `None` when the module is not coupled at all.
    #[inline]
    pub fn instability(&self, f: FileId) -> Option<f32> {
        let (ca, ce) = (self.ca(f), self.ce(f));
        let total = u64::from(ca) + u64::from(ce);
        (total > 0).then(|| (f64::from(ce) / total as f64) as f32)
    }

    /// Abstract classes over classes (0 without classes).
    #[inline]
    pub fn abstractness(&self, f: FileId) -> f32 {
        let c = self.classes(f);
        if c == 0 { 0.0 } else { (f64::from(self.abstract_classes(f)) / f64::from(c)) as f32 }
    }

    /// `|A + I - 1|`; `None` when the instability is undefined.
    #[inline]
    pub fn distance(&self, f: FileId) -> Option<f32> {
        self.instability(f).map(|i| (self.abstractness(f) + i - 1.0).abs())
    }

    /// Heap bytes (capacity).
    pub fn heap_bytes(&self) -> usize {
        4 * (self.ca.capacity()
            + self.ce.capacity()
            + self.classes.capacity()
            + self.abstracts.capacity()
            + self.symbols.capacity())
    }
}

/// Counters of one run.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CouplingStats {
    /// Files scanned.
    pub files: u64,
    /// Import edges counted (the sum of Ce).
    pub edges: u64,
    /// Classes whose WMC exceeds `max-class-wmc` (their call fan-out was computed).
    pub class_candidates: u64,
    /// Findings pushed.
    pub findings: u64,
}

/// Result of [`analyse`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CouplingReport {
    pub modules: ModuleCoupling,
    pub stats: CouplingStats,
}

/// Distinct callers of node `v` in call graph `cg`, `v` itself (recursion) excluded.
#[inline]
pub fn fan_in(cg: &Graph, v: NodeId) -> u32 {
    let p = cg.pred(v);
    p.len() as u32 - u32::from(p.binary_search(&v).is_ok())
}

/// Distinct callees of node `v` in call graph `cg`, `v` itself (recursion) excluded.
#[inline]
pub fn fan_out(cg: &Graph, v: NodeId) -> u32 {
    let s = cg.succ(v);
    s.len() as u32 - u32::from(s.binary_search(&v).is_ok())
}

/// Whether a file is a stub (`.pyi`).
fn is_stub(ix: &Index, f: FileId) -> bool {
    ix.file_path(f).as_os_str().as_encoded_bytes().ends_with(b".pyi")
}

/// Interned names that mark abstract classes (`None`: not in the program, never matches).
struct AbstractNames {
    abc: Option<Sym>,
    protocol: Option<Sym>,
    abc_meta: Option<Sym>,
    metaclass: Option<Sym>,
}

impl AbstractNames {
    fn of(ix: &Index) -> AbstractNames {
        let s = ix.strings();
        AbstractNames {
            abc: s.get(b"ABC"),
            protocol: s.get(b"Protocol"),
            abc_meta: s.get(b"ABCMeta"),
            metaclass: s.get(b"metaclass"),
        }
    }

    /// Whether base row `b` (store-absolute) of file view `fv` marks its class abstract.
    #[inline]
    fn base_is_abstract(&self, fv: &FileView<'_>, b: usize) -> bool {
        let Some(&last) = fv.base_chain(b).last() else { return false };
        let last = Some(last);
        match fv.store.bases.keyword[b] {
            None => last == self.abc || last == self.protocol,
            Some(kw) => Some(kw) == self.metaclass && last == self.abc_meta,
        }
    }
}

/// Whether an import edge from file `f` to file `g` counts (same scope; see the module docs).
#[inline]
fn same_scope(ix: &Index, f: FileId, g: FileId) -> bool {
    let (of, og) = (ix.file_origin(f), ix.file_origin(g));
    (of == Origin::FirstParty && og == Origin::FirstParty) || ix.file_group(f) == ix.file_group(g)
}

/// Computes the per-file coupling counts of `ix` (no findings).
pub fn module_coupling(ix: &Index, imp: &Imports) -> (ModuleCoupling, u64) {
    let n = ix.file_count() as usize;
    let mut mc = ModuleCoupling {
        ca: vec![0; n],
        ce: vec![0; n],
        classes: Vec::with_capacity(n),
        abstracts: Vec::with_capacity(n),
        symbols: Vec::with_capacity(n),
    };
    let mut edges = 0u64;
    for f in 0..n as FileId {
        if ix.file_node(f) == NONE || is_stub(ix, f) {
            continue;
        }
        let mut ce = 0u32;
        for e in imp.module_edges(f) {
            if e.flags & res_flags::TYPE_ONLY != 0 {
                continue;
            }
            let Some(g) = ix.file(e.dst) else { continue };
            if g == f || !same_scope(ix, f, g) {
                continue;
            }
            ce += 1;
            mc.ca[g as usize] += 1;
        }
        mc.ce[f as usize] = ce;
        edges += u64::from(ce);
    }
    let names = AbstractNames::of(ix);
    let defs = &ix.facts().defs;
    // Per def of the current file: an abstract class (reused across files).
    let mut abstract_def: Vec<bool> = Vec::new();
    for f in 0..n as FileId {
        let ff = ix.file_facts(f);
        let len = ff.defs.len as usize;
        let base = ff.defs.start as usize;
        abstract_def.clear();
        abstract_def.resize(len, false);
        let mut classes = 0u32;
        let fv = ix.file_view(f);
        for d in 1..len {
            let g = base + d;
            if defs.kind[g] == DefKind::Class as u8 {
                classes += 1;
                if defs.bases_len[g] > 0 && fv.bases(d as u32).any(|b| names.base_is_abstract(&fv, b)) {
                    abstract_def[d] = true;
                }
            } else if defs.flags[g] & def_flags::ABSTRACT != 0 {
                let p = defs.parent[g] as usize;
                if p < d && defs.kind[base + p] == DefKind::Class as u8 {
                    abstract_def[p] = true;
                }
            }
        }
        mc.classes.push(classes);
        mc.abstracts.push(abstract_def.iter().filter(|&&a| a).count() as u32);
        mc.symbols.push(len.saturating_sub(1) as u32);
    }
    (mc, edges)
}

/// The nearest class enclosing node `v` (`v` itself if it is a class), or `None` when `v` is
/// not inside a class. O(depth).
#[inline]
pub fn enclosing_class(ix: &Index, mut v: NodeId) -> Option<NodeId> {
    loop {
        match ix.kind(v) {
            NodeKind::Class => return Some(v),
            NodeKind::Function => v = ix.parent(v)?,
            _ => return None,
        }
    }
}

/// Distinct classes, other than `class` and the classes nested in it, containing a call
/// target of a node in `class`'s subtree (see the module docs). `scratch` is reused.
pub fn class_fan_out(ix: &Index, cg: &Graph, class: NodeId, scratch: &mut Vec<NodeId>) -> u32 {
    scratch.clear();
    for v in ix.subtree(class) {
        for &w in cg.succ(v) {
            if ix.is_descendant(class, w) {
                continue;
            }
            if let Some(c) = enclosing_class(ix, w) {
                scratch.push(c);
            }
        }
    }
    scratch.sort_unstable();
    scratch.dedup();
    scratch.len() as u32
}

/// Runs the coupling pass: appends the findings of the enabled PM2xx rules to `out` (in
/// FileId order, module findings before the file's god classes) and returns the per-file
/// counts, which are built even when no rule is enabled. Without a call graph (`calls`) the
/// god-class rule is skipped; when given, it must be a graph over the nodes of `ix` (see
/// [`crate::graph::call_graph`]).
pub fn analyse(ix: &Index, imp: &Imports, calls: Option<&Graph>, engine: &RuleEngine, out: &mut Findings) -> CouplingReport {
    if let Some(cg) = calls {
        assert_eq!(cg.node_count(), ix.node_count(), "call graph is not over the nodes of this index");
    }
    let (modules, edges) = module_coupling(ix, imp);
    let mut stats = CouplingStats { files: u64::from(ix.file_count()), edges, ..CouplingStats::default() };
    let before = out.len();
    if engine.should_run(Pass::Coupling) {
        let emit = Emit {
            fan_out: engine.is_enabled(HIGH_FAN_OUT),
            distance: engine.is_enabled(MAIN_SEQUENCE_DISTANCE),
            god_module: engine.is_enabled(GOD_MODULE),
            god_class: engine.is_enabled(GOD_CLASS) && calls.is_some(),
        };
        stats.class_candidates = findings(ix, &modules, calls, engine.thresholds(), emit, out);
    }
    stats.findings = (out.len() - before) as u64;
    CouplingReport { modules, stats }
}

/// Which rules emit.
#[derive(Clone, Copy)]
struct Emit {
    fan_out: bool,
    distance: bool,
    god_module: bool,
    god_class: bool,
}

/// Pushes the findings of first-party `.py` files; returns the number of god-class candidates.
fn findings(ix: &Index, mc: &ModuleCoupling, calls: Option<&Graph>, t: &Thresholds, emit: Emit, out: &mut Findings) -> u64 {
    let defs = &ix.facts().defs;
    let mx = ix.metrics();
    let mut scratch: Vec<NodeId> = Vec::new();
    let mut candidates = 0u64;
    for f in 0..ix.file_count() {
        let node = ix.file_node(f);
        if node == NONE || ix.file_origin(f) != Origin::FirstParty || is_stub(ix, f) {
            continue;
        }
        let file = crate::rules::FileId(f);
        let sym = SymbolId(node);
        let at = Location::new(file, 1, 1);
        let (ca, ce) = (mc.ca(f), mc.ce(f));
        let coupling = ca.saturating_add(ce);
        if emit.fan_out && ce > t.max_fan_out {
            out.push(HIGH_FAN_OUT, at, sym, 100, &[Arg::Symbol(sym), Arg::U32(ce), Arg::U32(t.max_fan_out)]);
        }
        if emit.distance
            && coupling >= t.distance_min_coupling
            && let (Some(d), Some(i)) = (mc.distance(f), mc.instability(f))
            && f64::from(d) > t.max_distance
        {
            let args = [
                Arg::Symbol(sym),
                Arg::F32(d),
                Arg::F32(t.max_distance as f32),
                Arg::F32(mc.abstractness(f)),
                Arg::F32(i),
            ];
            out.push(MAIN_SEQUENCE_DISTANCE, at, sym, 100, &args);
        }
        let symbols = mc.symbols(f);
        if emit.god_module && symbols > t.max_module_symbols && coupling > t.max_module_coupling {
            let args = [
                Arg::Symbol(sym),
                Arg::U32(symbols),
                Arg::U32(t.max_module_symbols),
                Arg::U32(coupling),
                Arg::U32(t.max_module_coupling),
            ];
            out.push(GOD_MODULE, at, sym, 100, &args);
        }
        let (Some(cg), true) = (calls, emit.god_class) else { continue };
        let ff = ix.file_facts(f);
        let base = ff.defs.start as usize;
        for d in 1..ff.defs.len as usize {
            let g = base + d;
            if defs.kind[g] != DefKind::Class as u8 {
                continue;
            }
            // One metrics row per def row except `<module>`, files in FileId order.
            let wmc = u32::from(mx.cyclomatic[base - f as usize + d - 1]);
            if wmc <= t.max_class_wmc {
                continue;
            }
            candidates += 1;
            let v = node + d as u32;
            let fan = class_fan_out(ix, cg, v, &mut scratch);
            if fan > t.max_class_fan_out {
                let cls = SymbolId(v);
                let at = Location::new(file, defs.name_line[g], defs.name_col[g] + 1);
                let args =
                    [Arg::Symbol(cls), Arg::U32(wmc), Arg::U32(t.max_class_wmc), Arg::U32(fan), Arg::U32(t.max_class_fan_out)];
                out.push(GOD_CLASS, at, cls, 100, &args);
            }
        }
    }
    candidates
}

