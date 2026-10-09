//! Cross-module dead code analysis (design: Checks / CLI, Dead code (a)-(g), (i); rules
//! PM401-PM405).
//!
//! ```text
//!   Index + Imports + ResolvedCalls + options (mode, entrypoints, presets, scripts)
//!     ├─> reach::roots          RootSet: <module> scopes, __main__, tests, entrypoint
//!     │                         decorators / bases / names, scripts, public API (library mode)
//!     ├─> seeds                 per def: a conditional "child seed" (dunder / protocol member,
//!     │                         unknown decorator, external base, dynamic module, exported,
//!     │                         library public) and same-name sibling rings
//!     ├─> liveness levels       one level 0..=100 per node: widest-path propagation from the
//!     │   (phase 1)             roots over call edges, members, bases, overrides (bucket queue)
//!     ├─> module levels         the same over packages/modules: imports, lifted calls, parents
//!     ├─> name fallback         one scan over the evidence hashes of every file, probing the
//!     │   (evidence)            names of the defs, modules and imports still in doubt
//!     ├─> levels (phase 2)      propagation continued from the evidence seeds
//!     └─> findings              PM401 / PM402 / PM403 per def, PM404 per module, PM405 per
//!                               import; confidence = 100 - level
//! ```
//!
//! # Liveness levels
//!
//! Instead of a live/dead bit, every node gets a *level*: how sure the analysis is that the node
//! runs (100 = certainly, 0 = no evidence at all). A finding's confidence is `100 - level`, so
//! `check`'s default gate (confidence >= 80) reports nodes whose level is at most 20.
//!
//! Levels propagate like a widest (bottleneck) path: a node is as live as its best path from a
//! root, a path is as live as its weakest step. Steps and their strengths:
//!
//! | step                                                   | level of the target            |
//! |--------------------------------------------------------|--------------------------------|
//! | root (see [`crate::reach::roots`])                     | 100                            |
//! | call edge, `certain` or `likely` (also decorators)     | level of the caller            |
//! | call edge, `possible` (unique-method fallback)         | min(caller, [`level::POSSIBLE`]) |
//! | class -> dunder / abstract / `@override` method, methods of a `Protocol`, nested class | level of the class |
//! | live method -> its class; live class -> its loaded bases | level of the method / class  |
//! | base method -> override (dynamic dispatch, design (b)) | min(base method, overriding class) |
//! | override -> the base method it overrides               | level of the override          |
//! | def -> same-name siblings (property setters, conditional redefinitions, overloads) | level of the def |
//! | scope -> child with an unknown decorator (may register it) | min(scope, [`level::DECORATED`]) |
//! | class with an unloaded base -> its methods (may override a framework method) | min(class, [`level::EXTERNAL_BASE`]) |
//! | module -> its defs when the module uses dynamic access (`getattr`, `globals()`, `vars()`, `locals()`, `__getattr__`, `__dict__`) | min(module, [`level::DYNAMIC`]) |
//! | application mode: module -> defs listed in `__all__` or re-exported | min(module, [`level::EXPORTED`]) |
//! | library mode: scope -> public defs that are not roots (private modules, outside `__all__`) | min(scope, [`level::LIBRARY_PUBLIC`]) |
//! | name fallback: the def's name occurs in its own file or in a file that imports it or its module | [`level::LINKED`] |
//! | name fallback: the name occurs anywhere else (attribute, identifier, string) | [`level::EVIDENCE`] |
//!
//! So, at the default gate, a def is reported when nothing reaches it, or when only a
//! library-mode public name keeps it (confidence 80). A function reached only through a
//! `possible` call, carrying an unknown decorator, or whose name appears elsewhere is reported
//! with confidence 50, 40 or 50 (visible with `--confidence 40`, JSON output, the GUI).
//!
//! Name evidence ([`crate::extract::facts`]: identifiers, attribute names and identifier-like
//! string segments of a file, as 32-bit hashes) cannot tell a call from a reference. An
//! occurrence in file `F` is therefore *explained* (ignored) when `F` holds a resolved
//! (`certain` / `likely`) call or decorator edge to the def, or a class whose resolved base is
//! the def: since the def is not live, that call comes from dead code and must not keep it
//! alive (dead call chains are reported in full). The def's own `__all__` entry is explained the
//! same way. A file that both calls a def from dead code and references it from live code is
//! the one case this misses.
//!
//! # Modules (PM404)
//!
//! Module levels use the same propagation over package/module nodes: roots are modules with a
//! non-`<module>` root kind or containing a non-`<module>` root (tests, `__main__`, scripts,
//! entrypoints, public API), and well-known script files ([`SCRIPT_NAMES`]); steps are imports
//! (any kind, `TYPE_CHECKING` included), calls lifted to modules (`possible` ones at
//! [`level::POSSIBLE`]) and module -> parent package (importing `a.b` runs `a`). A module
//! outside any regular package (top-level, or in a directory without `__init__.py`) may be run
//! as a script: [`level::SCRIPT_LIKE`]. The module's name in another file's evidence (for
//! instance `importlib.import_module("pkg.plugins.x")`) gives [`level::EVIDENCE`].
//!
//! # Imports (PM405)
//!
//! An import is used when its bound name (alias, imported name, or the first segment of a plain
//! `import a.b`) occurs in its file's evidence, when another import resolves through it (a
//! re-export: [`Imports::via`]), or when a file that imports its module mentions the name
//! (`mod.name`). Kept without a finding: star imports, `__future__`, `try`/`except ImportError`
//! fallbacks, names in `__all__`, the `import x as x` idiom and, in library mode, every
//! re-export of a package `__init__`. Confidence starts at 100 and loses: 30 when the file uses
//! dynamic access, 40 when another module star-imports the file (public names), 40 when the
//! name occurs in another file that does not import the module (string references such as
//! `mock.patch("pkg.mod.name")`), 30 for an application-mode `__init__` re-export, 20 in an
//! `__init__` file plus 20 more for a submodule import there (often for side effects), 10 for a
//! conditional import.
//!
//! # What is reported
//!
//! Defs, modules and imports of first-party `.py` files (stubs, shadowed and non-importable
//! files are skipped) with confidence at least [`MIN_EMIT`]. Not reported: `@overload`
//! signatures, defs under `if TYPE_CHECKING:`, dunder methods (live with their class) and
//! module-level dunder functions (`__getattr__`, `__dir__`: always live). A finding is dropped
//! when an enclosing def or the module has a finding of at least the same confidence (the
//! methods of an unused class, the defs of an unused module): deleting the container deletes
//! them. Defs are reported at their name; PM404 at line 1, column 1; PM405 at the imported
//! name. Suppressions, baselines and `per-file-ignores` apply through
//! [`RuleEngine::filter`](crate::rules::RuleEngine::filter) (design (i)); entrypoint `names`
//! globs are the whitelist.
//!
//! # Cost class: O(V + E) (`CostClass::Graph`)
//!
//! - Roots: see [`crate::reach::roots`] (O(files + defs + decorators)).
//! - Seeds: one pass over the def rows (decorator chains of decorated defs, the bases of
//!   classes for `Protocol`), the import rows (re-export targets) and two evidence lookups per
//!   file.
//! - Propagation: a bucket queue over the 101 levels. A node is expanded once per level
//!   improvement; levels only rise and take at most a dozen distinct values, so each node and
//!   edge is visited a bounded number of times. Per expansion: its call edges, its children
//!   with a seed, and for classes their loaded bases and overriding methods.
//! - Evidence: candidate names (defs and modules still below [`level::LINKED`], unused
//!   imports) go into a hash map; every hash of every first-party file's evidence is probed once
//!   (O(evidence)). Explained occurrences are checked by binary search in sorted
//!   `(candidate, file)` lists built from one pass over call edges, classes and import rows.
//! - Memory: two bytes per node (level and why), a labelled call graph without override edges
//!   (`10` bytes per edge plus `8` per node), the override edges (`8` bytes each), one seed
//!   byte per node, the module graphs, and the candidate tables. Nothing is allocated per edge
//!   during propagation; no string is built (names are compared as interned ids and hashes).
//!
//! See "Dead code (S4.3c)" in `docs/benchmarks.md` for measured costs.

mod evidence;
mod imports;
mod modules;
mod prop;
mod seeds;

#[cfg(test)]
mod tests;

use crate::config::{EntrypointMatcher, Mode};
use crate::extract::facts::{DefKind, def_flags, evidence_hash};
use crate::index::{FileId, Index, NONE, NodeId, NodeKind};
use crate::reach::{RootOptions, RootSet, is_library, roots};
use crate::resolve::{Imports, ResolvedCalls};
use crate::rules::codes::{UNUSED_CLASS, UNUSED_FUNCTION, UNUSED_IMPORT, UNUSED_METHOD, UNUSED_MODULE};
use crate::rules::{self, Arg, Findings, Location, Pass, RuleEngine, SymbolId};
use crate::timing;
use modules::ModuleProp;
use prop::{DefGraph, Levels};

pub use modules::SCRIPT_NAMES;

/// Liveness levels of the scoring table (see the module docs). Confidence is `100 - level`.
pub mod level {
    /// Certainly live: a root, or reached from one through sure steps.
    pub const LIVE: u8 = 100;
    /// Name fallback: the name occurs in the def's file or in a file importing it or its module.
    pub const LINKED: u8 = 90;
    /// Application mode: listed in `__all__` or re-exported.
    pub const EXPORTED: u8 = 70;
    /// Carries a decorator that may register it (not a known wrapper, not an entrypoint).
    pub const DECORATED: u8 = 60;
    /// A module outside any regular package: may be run as a script.
    pub const SCRIPT_LIKE: u8 = 60;
    /// Reached through a `possible` call edge (unique-method fallback).
    pub const POSSIBLE: u8 = 50;
    /// Method of a class with a base that is not loaded: may override a framework method.
    pub const EXTERNAL_BASE: u8 = 50;
    /// Name fallback: the name occurs in some other file.
    pub const EVIDENCE: u8 = 50;
    /// Defined in a module that uses dynamic access (`getattr`, `globals()`, ...).
    pub const DYNAMIC: u8 = 30;
    /// Library mode: a public name that is not part of the public API roots.
    pub const LIBRARY_PUBLIC: u8 = 20;
    /// No evidence of use.
    pub const DEAD: u8 = 0;
}

/// Findings below this confidence are not emitted (the node is almost certainly live).
pub const MIN_EMIT: u8 = 10;

/// Why a node has its level: the step that last raised it (see the module docs).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Why {
    /// Nothing reaches it.
    Dead = 0,
    /// A root ([`crate::reach::root_kind`]).
    Root = 1,
    /// A `certain` / `likely` call (or decorator) edge.
    Call = 2,
    /// A `possible` call edge.
    PossibleCall = 3,
    /// Live with its class: dunder, abstract, `@override` or `Protocol` method, nested class.
    Member = 4,
    /// A method of the class is live.
    Method = 5,
    /// A live subclass inherits from it.
    Subclass = 6,
    /// Overrides a live base method (or is overridden by a live method).
    Override = 7,
    /// A same-name sibling is live.
    Sibling = 8,
    /// Unknown decorator.
    Decorated = 9,
    /// Method of a class with an unloaded base.
    ExternalBase = 10,
    /// Defined in a module with dynamic access.
    Dynamic = 11,
    /// Application mode: in `__all__` or re-exported.
    Exported = 12,
    /// Library mode: a public name.
    LibraryPublic = 13,
    /// Name evidence in a linked file.
    Linked = 14,
    /// Name evidence elsewhere.
    Evidence = 15,
    /// Modules: imported by a live module.
    Import = 16,
    /// Modules: a live submodule.
    Submodule = 17,
    /// Modules: outside any regular package.
    ScriptLike = 18,
}

impl Why {
    const ALL: [Why; 19] = [
        Why::Dead,
        Why::Root,
        Why::Call,
        Why::PossibleCall,
        Why::Member,
        Why::Method,
        Why::Subclass,
        Why::Override,
        Why::Sibling,
        Why::Decorated,
        Why::ExternalBase,
        Why::Dynamic,
        Why::Exported,
        Why::LibraryPublic,
        Why::Linked,
        Why::Evidence,
        Why::Import,
        Why::Submodule,
        Why::ScriptLike,
    ];

    #[inline]
    pub fn from_u8(v: u8) -> Why {
        Why::ALL[(v as usize).min(Why::ALL.len() - 1)]
    }

    /// Strength of a child seed of this kind (the cap it puts on the child's level).
    #[inline]
    pub(crate) fn seed_strength(self) -> u8 {
        match self {
            Why::Member => level::LIVE,
            Why::Exported => level::EXPORTED,
            Why::Decorated => level::DECORATED,
            Why::ExternalBase => level::EXTERNAL_BASE,
            Why::Dynamic => level::DYNAMIC,
            Why::LibraryPublic => level::LIBRARY_PUBLIC,
            _ => 0,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Why::Dead => "dead",
            Why::Root => "root",
            Why::Call => "call",
            Why::PossibleCall => "possible-call",
            Why::Member => "member",
            Why::Method => "method",
            Why::Subclass => "subclass",
            Why::Override => "override",
            Why::Sibling => "sibling",
            Why::Decorated => "decorated",
            Why::ExternalBase => "external-base",
            Why::Dynamic => "dynamic",
            Why::Exported => "exported",
            Why::LibraryPublic => "library-public",
            Why::Linked => "linked",
            Why::Evidence => "evidence",
            Why::Import => "import",
            Why::Submodule => "submodule",
            Why::ScriptLike => "script-like",
        }
    }
}

/// Options of the dead-code analysis.
#[derive(Clone, Copy, Debug)]
pub struct DeadCodeOptions<'a> {
    /// Library vs application mode (`Auto` resolves with [`is_library`]).
    pub mode: Mode,
    /// Name-reference fallback through evidence hashes (`analysis.name-fallback`).
    pub name_fallback: bool,
    /// Entrypoint rules, presets included ([`crate::config::Config::entrypoint_matcher`]).
    pub entrypoints: Option<&'a EntrypointMatcher>,
    /// `[project.scripts]` / `entry_points.txt` targets (see [`RootOptions::scripts`]).
    pub scripts: &'a [&'a [u8]],
}

impl Default for DeadCodeOptions<'_> {
    fn default() -> Self {
        DeadCodeOptions { mode: Mode::Auto, name_fallback: true, entrypoints: None, scripts: &[] }
    }
}

impl<'a> DeadCodeOptions<'a> {
    /// Options of `config` with its compiled entrypoint matcher and the project's script
    /// targets (dropped when `entrypoints.project-scripts` is off).
    pub fn from_config(
        config: &crate::config::Config,
        entrypoints: &'a EntrypointMatcher,
        scripts: &'a [&'a [u8]],
    ) -> DeadCodeOptions<'a> {
        let scripts = if config.entrypoints.project_scripts { scripts } else { &[] };
        DeadCodeOptions {
            mode: config.mode,
            name_fallback: config.analysis.name_fallback,
            entrypoints: Some(entrypoints),
            scripts,
        }
    }
}

/// Counters of one run.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DeadCodeStats {
    /// Library mode was used.
    pub library: bool,
    pub roots: u64,
    /// Nodes at level 100.
    pub live: u64,
    /// Def / module / import candidates probed by the name fallback.
    pub candidates: u64,
    /// Evidence hashes probed.
    pub probes: u64,
    /// Node expansions of the propagation (defs and modules).
    pub expansions: u64,
    /// Findings per rule PM401..PM405 (before filtering).
    pub findings: [u64; 5],
    /// Heap bytes of the def graph (call and override graphs, seeds) at its peak.
    pub graph_bytes: u64,
}

/// Result of the dead-code analysis: a level per node and per module.
#[derive(Clone, Debug, Default)]
pub struct DeadCode {
    level: Vec<u8>,
    why: Vec<u8>,
    modules: modules::ModuleLevels,
    roots: RootSet,
    stats: DeadCodeStats,
}

impl DeadCode {
    /// Liveness level of node `v` (0..=100, see the module docs).
    #[inline]
    pub fn level(&self, v: NodeId) -> u8 {
        self.level[v as usize]
    }

    /// Confidence that `v` is dead (`100 - level`).
    #[inline]
    pub fn confidence(&self, v: NodeId) -> u8 {
        level::LIVE - self.level[v as usize]
    }

    /// The step that gave `v` its level.
    #[inline]
    pub fn why(&self, v: NodeId) -> Why {
        Why::from_u8(self.why[v as usize])
    }

    /// Module level of package/module node `v` (`None` for other nodes).
    pub fn module_level(&self, v: NodeId) -> Option<u8> {
        self.modules.level(v)
    }

    /// Why module `v` has its module level.
    pub fn module_why(&self, v: NodeId) -> Option<Why> {
        self.modules.why(v)
    }

    /// The roots the levels started from.
    pub fn roots(&self) -> &RootSet {
        &self.roots
    }

    pub fn stats(&self) -> &DeadCodeStats {
        &self.stats
    }

    /// Heap bytes (capacity).
    pub fn heap_bytes(&self) -> usize {
        self.level.capacity() + self.why.capacity() + self.modules.heap_bytes() + self.roots.heap_bytes()
    }
}

/// Whether file `f` is analysed: first-party, an importable runtime `.py` file.
pub fn analysed_file(ix: &Index, f: FileId) -> bool {
    ix.file_node(f) != NONE && crate::analysis::cycles::runtime_file(ix, f, true)
}

/// Def `v` (row `row` of file `f`) can be reported: not `@overload`, not under
/// `if TYPE_CHECKING:`, not a dunder.
#[inline]
fn reportable_def(ix: &Index, row: usize) -> bool {
    let flags = ix.facts().defs.flags[row];
    flags & (def_flags::OVERLOAD | def_flags::TYPE_CHECKING | def_flags::DUNDER) == 0
}

/// Runs the dead-code analysis (see the module docs) and appends the findings of the enabled
/// PM4xx rules to `out`. Does nothing (an empty result) unless the dead-code pass should run.
pub fn analyse(
    ix: &Index,
    imp: &Imports,
    calls: &ResolvedCalls,
    opts: &DeadCodeOptions<'_>,
    engine: &RuleEngine,
    out: &mut Findings,
) -> DeadCode {
    if !engine.should_run(Pass::DeadCode) {
        return DeadCode::default();
    }
    let emit = Emit {
        function: engine.is_enabled(UNUSED_FUNCTION),
        class: engine.is_enabled(UNUSED_CLASS),
        method: engine.is_enabled(UNUSED_METHOD),
        module: engine.is_enabled(UNUSED_MODULE),
        import: engine.is_enabled(UNUSED_IMPORT),
    };
    run(ix, imp, calls, opts, emit, out)
}

/// Which rules emit.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Emit {
    pub function: bool,
    pub class: bool,
    pub method: bool,
    pub module: bool,
    pub import: bool,
}

impl Emit {
    pub(crate) const ALL: Emit = Emit { function: true, class: true, method: true, module: true, import: true };
}

/// [`analyse`] with an explicit rule mask.
pub(crate) fn run(
    ix: &Index,
    imp: &Imports,
    calls: &ResolvedCalls,
    opts: &DeadCodeOptions<'_>,
    emit: Emit,
    out: &mut Findings,
) -> DeadCode {
    let library = is_library(ix, opts.mode, !opts.scripts.is_empty());
    let base = if library { RootOptions::library() } else { RootOptions::application() };
    let ropts = RootOptions { entrypoints: opts.entrypoints, scripts: opts.scripts, ..base };
    let roots = timing::time("deadcode.roots", || roots(ix, Some(imp), Some(calls), &ropts));
    let (g, info) = timing::time("deadcode.graph", || {
        let mut g = DefGraph::new(ix, calls);
        let info = seeds::fill(ix, imp, &mut g, library, |f| analysed_file(ix, f));
        (g, info)
    });
    let mut lv = Levels::new(ix.node_count());
    let mut mp = timing::time("deadcode.propagate", || {
        for v in roots.iter() {
            lv.raise(v, level::LIVE, Why::Root);
        }
        lv.run(&g);
        ModuleProp::new(ix, imp, calls, &roots, &info.included)
    });
    let mut stats = DeadCodeStats { library, roots: u64::from(roots.len()), ..DeadCodeStats::default() };
    let icands = if emit.import { imports::candidates(ix, imp, &info, library) } else { Vec::new() };
    let mut ihits = vec![evidence::import_hit::NONE; icands.len()];
    if opts.name_fallback {
        timing::time("deadcode.evidence", || {
            let defs = def_candidates(ix, &info.included, &lv.level);
            let mods = module_candidates(ix, &info.included, &mp);
            let keys: Vec<(u32, FileId)> =
                icands.iter().map(|c| (evidence_hash(ix.str(c.name)), c.file)).collect();
            let cands = evidence::Candidates { defs: &defs, modules: &mods, imports: &keys };
            let hits = evidence::scan(ix, imp, calls, &info.included, &cands);
            stats.candidates = (defs.len() + mods.len() + keys.len()) as u64;
            stats.probes = hits.probes;
            for (i, &d) in defs.iter().enumerate() {
                match hits.defs[i] {
                    0 => {}
                    level::LINKED => lv.raise(d, level::LINKED, Why::Linked),
                    l => lv.raise(d, l, Why::Evidence),
                }
            }
            for (i, &m) in mods.iter().enumerate() {
                if hits.modules[i] {
                    mp.raise(m, level::EVIDENCE, Why::Evidence);
                }
            }
            ihits = hits.imports;
            lv.run(&g);
        });
    }
    let (modules, mexp) = mp.finish();
    stats.graph_bytes = g.heap_bytes() as u64;
    drop(g);
    stats.expansions = lv.expansions + mexp;
    stats.live = lv.live_count();
    let mut dc = DeadCode { level: lv.level, why: lv.why, modules, roots, stats };
    timing::time("deadcode.findings", || {
        findings(ix, &info, &mut dc, emit, out);
        if emit.import {
            import_findings(ix, &icands, &ihits, &mut dc, out);
        }
    });
    dc
}

/// Reportable defs of included files below [`level::LINKED`], ascending.
fn def_candidates(ix: &Index, included: &[bool], level: &[u8]) -> Vec<NodeId> {
    let mut out = Vec::new();
    for f in 0..ix.file_count() {
        if !included[f as usize] {
            continue;
        }
        let ff = ix.file_facts(f);
        let m = ix.file_node(f);
        for d in 1..ff.defs.len {
            let v = m + d;
            if level[v as usize] < level::LINKED && reportable_def(ix, (ff.defs.start + d) as usize) {
                out.push(v);
            }
        }
    }
    out.sort_unstable();
    out
}

/// Modules of included files below [`level::EVIDENCE`], ascending.
fn module_candidates(ix: &Index, included: &[bool], mp: &ModuleProp<'_>) -> Vec<NodeId> {
    let mut out = Vec::new();
    for f in 0..ix.file_count() {
        let m = ix.file_node(f);
        if included[f as usize] && ix.kind(m) == NodeKind::Module && mp.level(m).is_some_and(|l| l < level::EVIDENCE) {
            out.push(m);
        }
    }
    out.sort_unstable();
    out
}

/// Confidence of the PM404 finding of file `f`'s module (0 if it has none).
fn module_confidence(ix: &Index, dc: &DeadCode, f: FileId) -> u8 {
    let m = ix.file_node(f);
    if ix.kind(m) != NodeKind::Module {
        return 0;
    }
    match dc.modules.level(m) {
        Some(l) if l < level::LIVE && level::LIVE - l >= MIN_EMIT => level::LIVE - l,
        _ => 0,
    }
}

/// PM401-PM404 findings, files in FileId order (module first, then defs in source order).
fn findings(ix: &Index, info: &seeds::FileInfo, dc: &mut DeadCode, emit: Emit, out: &mut Findings) {
    let defs = &ix.facts().defs;
    // Nearest reported ancestor defs: (subtree end, confidence).
    let mut stack: Vec<(NodeId, u8)> = Vec::new();
    for f in 0..ix.file_count() {
        if !info.included[f as usize] {
            continue;
        }
        let file = rules::FileId(f);
        let m = ix.file_node(f);
        let mconf = module_confidence(ix, dc, f);
        if mconf > 0 && emit.module {
            out.push(UNUSED_MODULE, Location::new(file, 1, 1), SymbolId(m), mconf, &[Arg::Symbol(SymbolId(m))]);
            dc.stats.findings[3] += 1;
        }
        let ff = ix.file_facts(f);
        stack.clear();
        for d in 1..ff.defs.len {
            let v = m + d;
            let row = (ff.defs.start + d) as usize;
            while stack.last().is_some_and(|&(end, _)| end <= v) {
                stack.pop();
            }
            let l = dc.level[v as usize];
            if l == level::LIVE || level::LIVE - l < MIN_EMIT || !reportable_def(ix, row) {
                continue;
            }
            let conf = level::LIVE - l;
            if mconf.max(stack.last().map_or(0, |&(_, c)| c)) >= conf {
                continue;
            }
            stack.push((ix.subtree(v).end, conf));
            let (rule, on, slot) = match DefKind::from_u8(defs.kind[row]) {
                DefKind::Class => (UNUSED_CLASS, emit.class, 1),
                _ if defs.flags[row] & def_flags::METHOD != 0 => (UNUSED_METHOD, emit.method, 2),
                _ => (UNUSED_FUNCTION, emit.function, 0),
            };
            if on {
                let at = Location::new(file, defs.name_line[row], defs.name_col[row] + 1);
                out.push(rule, at, SymbolId(v), conf, &[Arg::Symbol(SymbolId(v))]);
                dc.stats.findings[slot] += 1;
            }
        }
    }
}

/// PM405 findings, in candidate (file, row) order.
fn import_findings(
    ix: &Index,
    cands: &[imports::ImportCand],
    hits: &[u8],
    dc: &mut DeadCode,
    out: &mut Findings,
) {
    let st = &ix.facts().imports;
    for (c, &hit) in cands.iter().zip(hits) {
        let Some(conf) = imports::confidence(c, hit) else { continue };
        if conf < MIN_EMIT || module_confidence(ix, dc, c.file) >= conf {
            continue;
        }
        let row = c.row as usize;
        let at = Location::new(rules::FileId(c.file), st.line[row], st.col[row] + 1);
        out.push(UNUSED_IMPORT, at, SymbolId(ix.file_node(c.file)), conf, &[Arg::Name(c.name.to_raw())]);
        dc.stats.findings[4] += 1;
    }
}
