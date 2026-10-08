//! Import cycle analysis (design: Goals, Semantics; rule PM301 `import-cycle`).
//!
//! ```text
//!   Index + Imports ──> module import graph     first-party, runtime files only (no .pyi,
//!                       (import_graph_where)    no shadowed / non-importable files); edges
//!                                               whose rows are all TYPE_CHECKING dropped
//!                   ──> Scc (iterative Tarjan)  one cycle per cyclic component
//!                   ──> witness per component   BFS inside the component from its lowest
//!                                               module back to itself: a shortest cycle
//!                                               through that module
//!                   ──> PM301 per component     at the import that closes the witness
//! ```
//!
//! # Cost class: O(V + E)
//!
//! The graph build is two passes over the module edges of [`Imports`] (no temporary edge
//! list), Tarjan is linear, and the witness searches visit every module and edge of a cyclic
//! component at most once (each component is searched once, from one module). Scratch is one
//! `u32` per module (BFS parents, reset after each search) plus the BFS queue. Locating the
//! closing import scans the import rows of one file per cycle. Nothing is allocated per edge
//! or per module beyond these arrays; the result holds the members and witness of each cyclic
//! component (`4` bytes per module in a cycle, plus the witness).
//!
//! # Semantics
//!
//! - Nodes are package/module nodes of the index; an edge `a -> b` exists when a file of `a`
//!   imports `b` (see [`crate::resolve`]: `from pkg import sub` targets the submodule, symbols
//!   target the module they are imported from). Implicit parent-package imports (`import a.b`
//!   also runs `a/__init__`) are not edges, as in import-linter and pylint.
//! - Only edges of first-party files count by default ([`CycleOptions::first_party_only`]).
//!   Stub files (`.pyi`) never run and are skipped; so are shadowed and non-importable files.
//! - An edge is excluded when every import row behind it carries one of
//!   [`CycleOptions::exclude`]'s flags: by default [`res_flags::TYPE_ONLY`] (`if
//!   TYPE_CHECKING:` imports), unless `analysis.type-checking-cycles` is set. Function-level
//!   (lazy) imports still count: they are dependencies even when they do not run at import
//!   time.
//! - The witness of a component starts at its lowest node (deterministic) and is a shortest
//!   cycle through that node (not necessarily the shortest cycle of the component: finding
//!   that is O(V E)). It is rotated to start at the module whose import closes it, so a
//!   finding `a -> b -> c -> a` is located at `a`'s import of `b`.
//!
//! [`call_cycles`] runs the same search on the module-level call graph (no findings: for
//! queries and overlays).

#[cfg(test)]
pub(crate) mod tests;

use std::os::unix::ffi::OsStrExt;

use crate::config::Config;
use crate::graph::{CallFilter, Graph, ModuleGraph, Scc, import_graph_where, module_call_graph};
use crate::index::{FileId, Index, NONE, NodeId};
use crate::resolve::{Imports, ResolvedCalls, res_flags};
use crate::roots::{FileStatus, Origin};
use crate::rules::codes::IMPORT_CYCLE;
use crate::rules::{self, Arg, Findings, Location, SymbolId};

/// Which import edges the cycle search sees.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CycleOptions {
    /// An edge is dropped when every row behind it has one of these [`res_flags`]
    /// (`TYPE_ONLY` by default; `0` keeps every edge).
    pub exclude: u8,
    /// Only imports made by first-party files (the default).
    pub first_party_only: bool,
}

impl Default for CycleOptions {
    fn default() -> Self {
        CycleOptions { exclude: res_flags::TYPE_ONLY, first_party_only: true }
    }
}

impl CycleOptions {
    /// Options of `config`: `analysis.type-checking-cycles` keeps type-only edges.
    pub fn from_config(config: &Config) -> CycleOptions {
        let exclude = if config.analysis.type_checking_cycles { 0 } else { res_flags::TYPE_ONLY };
        CycleOptions { exclude, ..CycleOptions::default() }
    }
}

/// The cyclic components of a module graph: members and a witness cycle per component, as
/// index nodes. Components are in topological order of the condensation (importers first).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Cycles {
    /// Members of cycle `i`: `members[member_start[i]..member_start[i + 1]]`, ascending.
    members: Vec<NodeId>,
    member_start: Vec<u32>,
    /// Witness of cycle `i`, without repeating its first module at the end.
    witness: Vec<NodeId>,
    witness_start: Vec<u32>,
    /// Modules and edges of the searched graph.
    modules: u32,
    edges: u32,
}

impl Cycles {
    /// Number of cycles (cyclic components).
    #[inline]
    pub fn len(&self) -> usize {
        self.member_start.len().saturating_sub(1)
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Modules of cycle `i` (its strongly connected component), ascending.
    #[inline]
    pub fn members(&self, i: usize) -> &[NodeId] {
        &self.members[self.member_start[i] as usize..self.member_start[i + 1] as usize]
    }

    /// A concrete cycle through members of cycle `i`: each module imports (calls) the next,
    /// and the last one the first. A self loop is a one-module witness.
    #[inline]
    pub fn witness(&self, i: usize) -> &[NodeId] {
        &self.witness[self.witness_start[i] as usize..self.witness_start[i + 1] as usize]
    }

    /// Modules in the searched graph.
    #[inline]
    pub fn module_count(&self) -> u32 {
        self.modules
    }

    /// Edges in the searched graph.
    #[inline]
    pub fn edge_count(&self) -> u32 {
        self.edges
    }

    /// Heap bytes (capacity).
    pub fn heap_bytes(&self) -> usize {
        (self.members.capacity() + self.member_start.capacity() + self.witness.capacity() + self.witness_start.capacity()) * 4
    }
}

/// Cyclic components of `g` with a witness each, in local ids (see the module docs).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LocalCycles {
    pub members: Vec<u32>,
    pub member_start: Vec<u32>,
    pub witness: Vec<u32>,
    pub witness_start: Vec<u32>,
}

/// Finds every cyclic component of `g` and a witness cycle for each. O(V + E).
pub fn find_cycles(g: &Graph) -> LocalCycles {
    let scc = Scc::of(g);
    let mut out = LocalCycles { member_start: vec![0], witness_start: vec![0], ..LocalCycles::default() };
    // BFS parents (NONE = unvisited), reset after each search through the queue.
    let mut parent: Vec<u32> = Vec::new();
    let mut queue: Vec<u32> = Vec::new();
    for c in 0..scc.count() {
        if !scc.is_cyclic(g, c) {
            continue;
        }
        if parent.is_empty() {
            parent = vec![NONE; g.node_count() as usize];
        }
        let members = scc.members(c);
        out.members.extend_from_slice(members);
        out.member_start.push(out.members.len() as u32);
        witness(g, &scc, c, members[0], &mut parent, &mut queue, &mut out.witness);
        out.witness_start.push(out.witness.len() as u32);
    }
    out
}

/// Appends a shortest cycle through `s` inside component `c`, rotated to start at the node
/// whose edge closes it (`u -> s`): `u, s, ..., parent(u)`.
fn witness(g: &Graph, scc: &Scc, c: u32, s: u32, parent: &mut [u32], queue: &mut Vec<u32>, out: &mut Vec<u32>) {
    if g.has_edge(s, s) {
        out.push(s);
        return;
    }
    queue.clear();
    queue.push(s);
    parent[s as usize] = s;
    let mut head = 0;
    let mut closing = NONE;
    'bfs: while head < queue.len() {
        let u = queue[head];
        head += 1;
        for &w in g.succ(u) {
            if w == s {
                closing = u;
                break 'bfs;
            }
            if parent[w as usize] == NONE && scc.comp(w) == c {
                parent[w as usize] = u;
                queue.push(w);
            }
        }
    }
    // `s` is in a cyclic component without a self loop: some member reaches it back.
    debug_assert!(closing != NONE);
    if closing != NONE {
        // `closing, parent(closing), ..., s` reversed is `s .. closing`; emit `closing` first.
        out.push(closing);
        let base = out.len();
        let mut v = closing;
        while v != s {
            v = parent[v as usize];
            out.push(v);
        }
        out[base..].reverse();
    }
    for &v in queue.iter() {
        parent[v as usize] = NONE;
    }
}

impl Cycles {
    /// Maps local cycles of `mg` to index nodes.
    fn from_local(mg: &ModuleGraph, lc: LocalCycles) -> Cycles {
        let LocalCycles { mut members, member_start, mut witness, witness_start } = lc;
        for m in members.iter_mut().chain(witness.iter_mut()) {
            *m = mg.node(*m);
        }
        Cycles { members, member_start, witness, witness_start, modules: mg.len(), edges: mg.graph().edge_count() }
    }
}

/// Whether file `f` contributes import edges to the cycle search: an importable runtime file
/// (not a stub, not shadowed), first-party unless `first_party_only` is off.
pub fn runtime_file(ix: &Index, f: FileId, first_party_only: bool) -> bool {
    matches!(ix.file_status(f), FileStatus::Active | FileStatus::StubShadowed)
        && !(first_party_only && ix.file_origin(f) != Origin::FirstParty)
        && !ix.file_path(f).as_os_str().as_bytes().ends_with(b".pyi")
}

/// The module import graph searched by [`import_cycles`].
pub fn cycle_graph(ix: &Index, imp: &Imports, opts: &CycleOptions) -> ModuleGraph {
    // Once per file rather than per edge (the builder visits every edge twice).
    let keep: Vec<bool> = (0..ix.file_count()).map(|f| runtime_file(ix, f, opts.first_party_only)).collect();
    import_graph_where(ix, imp, opts.exclude, |f| keep[f as usize])
}

/// Import cycles of `ix` (see the module docs). O(V + E).
pub fn import_cycles(ix: &Index, imp: &Imports, opts: &CycleOptions) -> Cycles {
    let mg = cycle_graph(ix, imp, opts);
    let lc = find_cycles(mg.graph());
    Cycles::from_local(&mg, lc)
}

/// Cycles of the module-level call graph (calls the filter keeps, calls within one module
/// ignored). No findings: for queries and overlays. O(V + E) after call resolution.
pub fn call_cycles(ix: &Index, calls: &ResolvedCalls, filter: CallFilter) -> Cycles {
    let mg = module_call_graph(ix, calls, filter);
    let lc = find_cycles(mg.graph());
    Cycles::from_local(&mg, lc)
}

/// Location of the first import of `dst` by file `f` whose flags avoid `exclude` (falling
/// back to the first import of `dst`); `None` if `f` does not import `dst`.
pub fn import_location(ix: &Index, imp: &Imports, f: FileId, dst: NodeId, exclude: u8) -> Option<Location> {
    let st = ix.facts();
    let mut fallback = None;
    for row in ix.file_facts(f).imports.range() {
        if imp.module_node(row) != dst {
            continue;
        }
        let loc = Location::new(rules::FileId(f), st.imports.line[row], st.imports.col[row] + 1);
        if imp.flags(row) & exclude == 0 {
            return Some(loc);
        }
        fallback = fallback.or(Some(loc));
    }
    fallback
}

/// Appends one PM301 finding per cycle of `cycles` to `out`: at the import closing its
/// witness, `import cycle: a -> b -> ... -> a`, symbol the first module of the witness.
pub fn findings(ix: &Index, imp: &Imports, cycles: &Cycles, opts: &CycleOptions, out: &mut Findings) {
    let mut args: Vec<Arg> = Vec::new();
    for i in 0..cycles.len() {
        let w = cycles.witness(i);
        let (a, b) = (w[0], w[1 % w.len()]);
        let loc = ix
            .file(a)
            .and_then(|f| import_location(ix, imp, f, b, opts.exclude))
            .unwrap_or(Location::new(ix.file(a).map_or(rules::FileId::NONE, rules::FileId), 0, 0));
        args.clear();
        args.extend(w.iter().map(|&v| Arg::Symbol(SymbolId(v))));
        args.push(Arg::Symbol(SymbolId(a)));
        out.push(IMPORT_CYCLE, loc, SymbolId(a), 100, &args);
    }
}

/// Runs the import cycle analysis and appends its findings to `out`.
pub fn analyse(ix: &Index, imp: &Imports, opts: &CycleOptions, out: &mut Findings) -> Cycles {
    let cycles = import_cycles(ix, imp, opts);
    findings(ix, imp, &cycles, opts, out);
    cycles
}
