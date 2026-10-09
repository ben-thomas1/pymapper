//! Findings for the overlays and the findings panel: the analyses of `pymapper check`, run
//! once in the background after loading, mapped to nodes and aggregated for badges.
//!
//! ```text
//!   RuleEngine ─> hotspot, cycles, layers, coupling (+ the query's call graph), dead code   (as check)
//!             ─> suppressions + per-file-ignores filter (below-confidence findings kept:
//!                the panel's confidence slider filters them)
//!             ─> node per finding (its symbol, else the innermost def at its line)
//!             ─> badges: per node with findings in its subtree, (direct, subtree, worst
//!                severity) — one ancestor walk per finding
//! ```
//!
//! Duplication (PM501) is not run: its fingerprints are only taken during an index load that
//! asks for them, and the GUI's load does not (it would cost every start). The suppression and
//! per-file-ignore steps mirror `pymapper-cli/src/pipeline.rs` and `cmd/check.rs` (the GUI
//! cannot depend on the CLI crate).

use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use pymapper_core::analysis::deadcode::{self, DeadCodeOptions};
use pymapper_core::analysis::{coupling, cycles, hotspot, layers};
use pymapper_core::config::Config;
use pymapper_core::extract::facts::sup_kind;
use pymapper_core::index::{Index, NodeId};
use pymapper_core::intern::Sym;
use pymapper_core::query::Query;
use pymapper_core::rules::codes::{GOD_CLASS, IMPORT_CYCLE};
use pymapper_core::rules::{self, Arg, FileId, FilterStats, Finding, Findings, Pass, RuleEngine, Severity, SymbolId, Suppressions, SuppressionsBuilder};
use pymapper_core::walk::{Gitignore, Match};

use crate::codemap::load::Analysis;

/// "No node" in [`Insights::nodes`].
pub const NO_NODE: NodeId = NodeId::MAX;

/// Findings in a subtree, for a badge.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Badge {
    pub node: NodeId,
    /// Findings on the node itself.
    pub direct: u32,
    /// Findings on the node and its descendants.
    pub subtree: u32,
    /// Worst severity in the subtree.
    pub worst: Severity,
    /// Worst severity of the node's own findings (meaningful when `direct > 0`).
    pub worst_direct: Severity,
}

/// One import contract violation between two loaded modules.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LayerViolation {
    /// Index into [`Insights::findings`].
    pub finding: u32,
    pub importer: NodeId,
    pub imported: NodeId,
}

/// What the findings pass produced.
#[derive(Default)]
pub struct Insights {
    /// Filtered findings, worst first (severity, then confidence, then location).
    pub findings: Findings,
    /// Node of each finding ([`NO_NODE`] when it has none).
    pub nodes: Vec<NodeId>,
    /// Indices of the findings with a node, ordered by (node, index): the findings of one
    /// node are a contiguous run (see [`Self::on_node`]).
    pub by_node: Vec<u32>,
    /// Badges at the configured `min-confidence`.
    pub badges: Arc<Badges>,
    /// Indices into `findings` of import cycles (PM301).
    pub cycles: Vec<u32>,
    /// Import contract violations whose imported module is loaded.
    pub layer_violations: Vec<LayerViolation>,
    pub filter: FilterStats,
    /// Analyses that ran, for the panel.
    pub passes: Vec<&'static str>,
    pub elapsed: Duration,
}

impl std::fmt::Debug for Insights {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Insights").field("findings", &self.findings.len()).field("badges", &self.badges.list.len()).finish_non_exhaustive()
    }
}

impl Insights {
    /// Heap bytes (approximate: the findings' argument buffers are not counted).
    pub fn heap_bytes(&self) -> usize {
        self.findings.len() * std::mem::size_of::<Finding>()
            + (self.nodes.capacity() + self.by_node.capacity()) * 4
            + self.badges.heap_bytes()
            + (self.cycles.capacity() + self.layer_violations.capacity()) * 4
    }

    /// Indices of the findings on node `v` exactly, worst first. O(log findings).
    pub fn on_node(&self, v: NodeId) -> impl Iterator<Item = u32> + '_ {
        let lo = self.by_node.partition_point(|&i| self.nodes[i as usize] < v);
        let hi = lo + self.by_node[lo..].partition_point(|&i| self.nodes[i as usize] == v);
        self.by_node[lo..hi].iter().copied()
    }

    /// The badges of the findings with confidence >= `min_conf`. O(findings x depth) plus a
    /// transient 9 bytes per node.
    pub fn badges_at(&self, ix: &Index, min_conf: u8) -> Badges {
        Badges { min_conf, list: badges(ix, &self.findings, &self.nodes, min_conf) }
    }
}

/// Nodes with findings in their subtree, ascending by node, counting findings with confidence
/// >= `min_conf`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Badges {
    pub min_conf: u8,
    pub list: Vec<Badge>,
}

impl Badges {
    /// The badge of node `v`. O(log badges).
    pub fn get(&self, v: NodeId) -> Option<&Badge> {
        self.list.binary_search_by_key(&v, |b| b.node).ok().map(|i| &self.list[i])
    }

    pub fn heap_bytes(&self) -> usize {
        self.list.capacity() * std::mem::size_of::<Badge>()
    }
}

/// Runs the enabled analyses over `a` (see the module docs).
pub fn analyse(a: &Analysis, q: &Query<'_>) -> Insights {
    let t = Instant::now();
    let config = &a.config;
    let (ix, imp) = (a.index(), a.imports());
    let engine = RuleEngine::new(config);
    let mut out = Findings::new();
    let mut passes = Vec::new();
    if engine.should_run(Pass::Hotspot) {
        hotspot::analyse(ix, &engine, &mut out);
        passes.push("complexity");
    }
    if engine.should_run(Pass::Cycles) {
        cycles::analyse(ix, imp, &cycles::CycleOptions::from_config(config), &mut out);
        passes.push("import cycles");
    }
    if engine.should_run(Pass::Layers) && !config.contracts.is_empty() {
        layers::analyse(ix, imp, &config.contracts, &mut out);
        passes.push("contracts");
    }
    if engine.should_run(Pass::Coupling) {
        // The query's call graph has exactly `call_graph(.., CallFilter::default())`'s edges
        // (every edge with a target, parallel edges folded); coupling reads only adjacency.
        let cg = engine.is_enabled(GOD_CLASS).then(|| q.call_graph());
        coupling::analyse(ix, imp, cg, &engine, &mut out);
        passes.push("coupling");
    }
    if engine.should_run(Pass::DeadCode) {
        let matcher = config.entrypoint_matcher();
        let scripts: Vec<&[u8]> = a.units.all_scripts().iter().map(|s| s.as_bytes()).collect();
        let opts = DeadCodeOptions::from_config(config, &matcher, Some(&a.units), &scripts).with_config_evidence(a.config_evidence.as_ref());
        deadcode::analyse(ix, imp, a.calls(), &opts, &engine, &mut out);
        passes.push("dead code");
    }
    let sup = suppressions(ix);
    let per_file = per_file_ignores(config);
    let filter = engine.filter(&mut out, &sup, true, |f, i| per_file.get(i).and_then(Option::as_ref).is_some_and(|gi| per_file_matches(ix, &a.root, gi, f)));
    drop(sup);
    // Worst first, then by location (deterministic).
    out.sort_by(|x, y| {
        let sev = |f: &Finding| std::cmp::Reverse(f.rule.def().severity);
        (sev(x), std::cmp::Reverse(x.confidence), x.loc, x.rule, x.symbol).cmp(&(sev(y), std::cmp::Reverse(y.confidence), y.loc, y.rule, y.symbol))
    });
    let nodes: Vec<NodeId> = out.as_slice().iter().map(|f| node_of(ix, q, f)).collect();
    let mut cycles_ix = Vec::new();
    let mut layer_ix = Vec::new();
    for (i, (f, args)) in out.iter().enumerate() {
        if f.rule == IMPORT_CYCLE {
            cycles_ix.push(i as u32);
        } else if f.rule.def().pass == Pass::Layers
            && let (Some(&Arg::Symbol(src)), Some(dst)) = (args.first(), args.get(1).and_then(|a| module_of(ix, *a)))
            && src.0 < ix.node_count()
        {
            layer_ix.push(LayerViolation { finding: i as u32, importer: src.0, imported: dst });
        }
    }
    let min_conf = config.min_confidence;
    let badges = Arc::new(Badges { min_conf, list: badges(ix, &out, &nodes, min_conf) });
    let mut by_node: Vec<u32> = (0..nodes.len() as u32).filter(|&i| nodes[i as usize] != NO_NODE).collect();
    by_node.sort_unstable_by_key(|&i| (nodes[i as usize], i));
    Insights { findings: out, nodes, by_node, badges, cycles: cycles_ix, layer_violations: layer_ix, filter, passes, elapsed: t.elapsed() }
}

/// The module an argument names: a symbol, or an interned dotted name of a loaded module.
fn module_of(ix: &Index, a: Arg) -> Option<NodeId> {
    match a {
        Arg::Symbol(s) if s.0 < ix.node_count() => Some(s.0),
        Arg::Name(raw) => {
            let s = Sym::from_raw(raw).filter(|s| s.index() < ix.strings().len())?;
            ix.module_node(ix.str(s), 0)
        }
        _ => None,
    }
}

/// The node a finding is about: its symbol, else the innermost def at its line.
fn node_of(ix: &Index, q: &Query<'_>, f: &Finding) -> NodeId {
    if f.symbol != SymbolId::NONE && f.symbol.0 < ix.node_count() {
        return f.symbol.0;
    }
    if f.loc.file != FileId::NONE && f.loc.file.0 < ix.file_count() {
        return q.symbol_at(f.loc.file.0, f.loc.line.max(1));
    }
    NO_NODE
}

/// Badges of the findings at or above `min_conf` (see [`Badge`]).
fn badges(ix: &Index, findings: &Findings, nodes: &[NodeId], min_conf: u8) -> Vec<Badge> {
    let n = ix.node_count() as usize;
    let mut direct: Vec<u32> = Vec::new();
    let mut subtree: Vec<u32> = Vec::new();
    // Severity + 1 per node (0 = none): subtree worst in the low nibble, direct in the high.
    let mut sev: Vec<u8> = Vec::new();
    let mut touched: Vec<NodeId> = Vec::new();
    for (f, &v) in findings.as_slice().iter().zip(nodes) {
        if v == NO_NODE || f.confidence < min_conf {
            continue;
        }
        if direct.is_empty() {
            direct.resize(n, 0);
            subtree.resize(n, 0);
            sev.resize(n, 0);
        }
        let s = f.rule.def().severity as u8 + 1;
        direct[v as usize] += 1;
        let hi = (sev[v as usize] >> 4).max(s);
        sev[v as usize] = (hi << 4) | (sev[v as usize] & 0x0f);
        let mut cur = Some(v);
        while let Some(u) = cur {
            let i = u as usize;
            if subtree[i] == 0 {
                touched.push(u);
            }
            subtree[i] += 1;
            sev[i] = (sev[i] & 0xf0) | (sev[i] & 0x0f).max(s);
            cur = ix.parent(u);
        }
    }
    touched.sort_unstable();
    let sev_of = |x: u8| match x {
        3 => Severity::Error,
        2 => Severity::Warning,
        _ => Severity::Info,
    };
    touched
        .iter()
        .map(|&v| {
            let i = v as usize;
            Badge { node: v, direct: direct[i], subtree: subtree[i], worst: sev_of(sev[i] & 0x0f), worst_direct: sev_of(sev[i] >> 4) }
        })
        .collect()
}

/// Inline `# pymapper: ignore[...]` and F401 `# noqa` suppressions of every file (as the CLI
/// pipeline; malformed codes suppress nothing).
pub fn suppressions(ix: &Index) -> Suppressions {
    let mut b = SuppressionsBuilder::new();
    let st = &ix.facts().suppressions;
    let mut entries = Vec::new();
    for f in 0..ix.file_count() {
        let range = ix.file_facts(f).suppressions.range();
        if range.is_empty() {
            continue;
        }
        entries.clear();
        let view = ix.file_view(f);
        for r in range {
            let line = st.line[r];
            if st.kind[r] == sup_kind::NOQA_F401 {
                entries.push(rules::noqa_f401(line));
                continue;
            }
            if st.kind[r] == sup_kind::ALL {
                rules::parse_codes(line, None, &mut entries, |_| {});
            }
            for &s in view.chain(st.codes[r]) {
                rules::parse_codes(line, Some(ix.str(s)), &mut entries, |_| {});
            }
        }
        b.push_file(FileId(f), &entries);
    }
    b.build()
}

/// `per-file-ignores` patterns compiled once each (`None`: an invalid pattern, which ignores
/// nothing here; `pymapper check` reports it).
fn per_file_ignores(config: &Config) -> Vec<Option<Gitignore>> {
    config
        .rules
        .per_file_ignores
        .iter()
        .map(|e| Gitignore::check_pattern(e.pattern.as_bytes()).ok().map(|()| Gitignore::from_patterns(&[e.pattern.as_bytes()])))
        .collect()
}

/// Whether a `per-file-ignores` pattern matches file `f` (relative to the project root, or a
/// parent directory of it), as `pymapper check`.
fn per_file_matches(ix: &Index, root: &Path, gi: &Gitignore, f: FileId) -> bool {
    if f.0 >= ix.file_count() {
        return false;
    }
    let p = ix.file_path(f.0).as_os_str().as_bytes();
    let rel = match p.strip_prefix(root.as_os_str().as_bytes()) {
        Some([b'/', rest @ ..]) => rest,
        _ => p.strip_prefix(b"/").unwrap_or(p),
    };
    for (i, &c) in rel.iter().enumerate() {
        if c == b'/' && gi.matched(&rel[..i], true) == Match::Ignore {
            return true;
        }
    }
    gi.matched(rel, false) == Match::Ignore
}
