//! Query API: symbol lookup, callers, callees, impact, path, deps, cycles, summary (design:
//! Checks / CLI, "Queries"). The same API backs the CLI subcommands and the GUI.
//!
//! ```text
//!   Index + Imports + ResolvedCalls ──> Query::new     call graph over NodeIds (forward +
//!                                       │              reverse CSR, label = best confidence
//!                                       │              and an "override only" bit), built once
//!                                       ├─ lazily ──> NameIndex      nodes sorted by name Sym
//!                                       │                            (LSD radix, 4 B per node)
//!                                       ├─ lazily ──> import graph   every module edge
//!                                       └─ lazily ──> cycle graph    first-party runtime
//!                                                                    imports + their SCCs
//!   Query + Scratch ──> lookup / callers / callees / impact / path / deps / cycles / summary
//! ```
//!
//! # Zero copy
//!
//! Results hold [`NodeId`]s and small `Copy` rows, never strings: names, qualified ids, paths
//! and lines are read from the index on demand through [`SymbolRef`] (`&'a [u8]` / `&'a Path`
//! borrowed from the index) or rendered into a caller buffer ([`Query::qid`]). Direct
//! neighbours are iterators over the CSR rows ([`Query::direct_callers`]), with no allocation.
//!
//! # Scratch
//!
//! Traversals take a [`Scratch`]: visit marks stamped with a generation counter (no clearing
//! between queries) and per-node positions, allocated on first use (8 bytes per node per
//! side; the zeroed pages are only touched where a query goes). Keep one per thread and reuse
//! it: a query then allocates only its result.
//!
//! # Targets
//!
//! A query target is a node. With [`Scope::Auto`] (the default) a function means itself and
//! any other node its whole subtree (a class: its methods; a module: its `<module>` scope and
//! every def; a package: every module below it). Subtrees are pre-order ranges, so "is in the
//! target" is two compares and edges inside the target are never reported.
//!
//! # Costs
//!
//! | query                      | cost                                                        |
//! |----------------------------|-------------------------------------------------------------|
//! | `Query::new`               | O(V + E) (two passes over the call edges, no edge list)     |
//! | `lookup` exact / qualified | one hash lookup + a scan of the children per def segment    |
//! | `lookup` suffix            | O(log V + candidates x depth) after the name index (O(V))   |
//! | `lookup` fuzzy             | O(distinct names) with a length filter before edit distance |
//! | `callers` / `callees`      | O(visited nodes + their edges), bounded by depth and limit  |
//! | `impact`                   | `callers` without a depth bound + O(k log k) rollup         |
//! | `path`                     | bidirectional BFS: O(visited) on the smaller frontier side  |
//! | `deps`                     | O(target subtree + edges of the files on both sides)        |
//! | `cycles` (imports)         | O(modules + import edges) once (lazy), then O(component)    |
//! | `cycles` (calls)           | O(forward reach of the target) per query                    |
//! | `summary`                  | O(V + E) (one pass over nodes and call rows, Tarjan)        |
//!
//! None is quadratic. Memory beyond the index: the call graph (`8 (V + 1) + 10 E` bytes,
//! forward and reverse CSR with labels), the name index (`4 V`), the module graphs when used
//! and the scratch (`8 V` per side used).

mod cycles;
mod deps;
mod lookup;
mod path;
mod summary;
mod walk;

#[cfg(test)]
mod tests;

pub use cycles::CycleHit;
pub use deps::{DepOptions, Dependency, Deps, Direction, ExternalDep};
pub use lookup::{LookupOptions, Match, MatchKind, NameIndex, Resolved};
pub use path::{PathGraph, PathOptions, ShortestPath};
pub use summary::{OriginCounts, Ranked, Summary, SummaryOptions};
pub use walk::{Hop, Impact, Walk, WalkOptions};

use std::ops::Range;
use std::path::Path;
use std::sync::OnceLock;

use crate::analysis::cycles::{CycleOptions, cycle_graph};
use crate::graph::{Csr, Graph, ModuleGraph, Scc, import_graph};
use crate::index::{FileId, Index, NodeId, NodeKind};
use crate::resolve::{Confidence, Imports, Reason, ResolvedCalls};
use crate::roots::Origin;

/// Bit 0 of a call graph label: every call edge behind it with the best confidence is an
/// override edge (base method -> override, dynamic dispatch).
const LABEL_OVERRIDE: u8 = 1;

/// Label of a call edge in the query call graph: `conf << 1 | override`. Parallel edges keep
/// the minimum: the best confidence, preferring a real call over an override at equal
/// confidence.
#[inline]
fn label(conf: Confidence, reason: Reason) -> u8 {
    ((conf as u8) << 1) | u8::from(reason == Reason::Override)
}

/// Confidence of a call graph label.
#[inline]
fn label_conf(l: u8) -> Confidence {
    Confidence::from_u8(l >> 1)
}

/// What a query target covers (see the module docs).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Scope {
    /// A function: itself; anything else: its subtree.
    #[default]
    Auto,
    /// The node only.
    Node,
    /// The node and every descendant.
    Subtree,
}

/// Options fixed for the life of a [`Query`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct QueryOptions {
    /// Import edges searched for cycles (as `pymapper check`; see [`CycleOptions`]).
    pub cycles: CycleOptions,
}

/// Read-only query state over one loaded program. `Sync`: share it between threads, with one
/// [`Scratch`] per thread.
pub struct Query<'a> {
    ix: &'a Index,
    imp: &'a Imports,
    calls: &'a ResolvedCalls,
    opts: QueryOptions,
    /// Every call edge with a target; labels as [`label`].
    graph: Graph,
    names: OnceLock<NameIndex>,
    imports: OnceLock<ModuleGraph>,
    cycle: OnceLock<(ModuleGraph, Scc)>,
}

impl<'a> Query<'a> {
    /// Builds the call graph (every call edge with a target, override edges included). O(V + E).
    pub fn new(ix: &'a Index, imp: &'a Imports, calls: &'a ResolvedCalls) -> Query<'a> {
        Query::with_options(ix, imp, calls, QueryOptions::default())
    }

    pub fn with_options(ix: &'a Index, imp: &'a Imports, calls: &'a ResolvedCalls, opts: QueryOptions) -> Query<'a> {
        let edges = calls.edges();
        let graph = Graph::build_labelled(ix.node_count(), || {
            edges.iter().filter(|e| e.conf != Confidence::Unresolved).map(|e| (e.src, e.dst, label(e.conf, e.reason)))
        });
        Query { ix, imp, calls, opts, graph, names: OnceLock::new(), imports: OnceLock::new(), cycle: OnceLock::new() }
    }

    #[inline]
    pub fn index(&self) -> &'a Index {
        self.ix
    }

    #[inline]
    pub fn imports(&self) -> &'a Imports {
        self.imp
    }

    #[inline]
    pub fn calls(&self) -> &'a ResolvedCalls {
        self.calls
    }

    /// The call graph over node ids (labels: see the module source; use [`Query::direct_callers`]
    /// / [`Query::direct_callees`] for typed access).
    #[inline]
    pub fn call_graph(&self) -> &Graph {
        &self.graph
    }

    /// The name index, built on first use (O(V), `4 V` bytes).
    pub fn names(&self) -> &NameIndex {
        self.names.get_or_init(|| NameIndex::build(self.ix))
    }

    /// The module import graph with every edge (type-only, lazy and conditional included;
    /// labels are the edge flags), built on first use.
    pub fn import_graph(&self) -> &ModuleGraph {
        self.imports.get_or_init(|| import_graph(self.ix, self.imp, 0))
    }

    /// The import graph searched for cycles and its SCCs, built on first use.
    fn cycle_graph(&self) -> &(ModuleGraph, Scc) {
        self.cycle.get_or_init(|| {
            let mg = cycle_graph(self.ix, self.imp, &self.opts.cycles);
            let scc = Scc::of(mg.graph());
            (mg, scc)
        })
    }

    /// Heap bytes (capacity) of the query state: the call graph plus the lazy tables built so
    /// far (the index and resolutions are borrowed, not counted).
    pub fn heap_bytes(&self) -> usize {
        self.graph.heap_bytes()
            + self.names.get().map_or(0, NameIndex::heap_bytes)
            + self.imports.get().map_or(0, ModuleGraph::heap_bytes)
            + self.cycle.get().map_or(0, |(mg, scc)| mg.heap_bytes() + scc.heap_bytes())
    }

    /// A view of node `v` with borrowed accessors.
    #[inline]
    pub fn symbol(&self, v: NodeId) -> SymbolRef<'a> {
        SymbolRef { ix: self.ix, node: v }
    }

    /// Appends the qualified id of `v` to `out`.
    #[inline]
    pub fn qid(&self, v: NodeId, out: &mut Vec<u8>) {
        self.ix.qid(v, out);
    }

    /// The node range a target covers (see [`Scope`]).
    pub fn target(&self, v: NodeId, scope: Scope) -> Range<NodeId> {
        let whole = match scope {
            Scope::Node => false,
            Scope::Subtree => true,
            Scope::Auto => self.ix.kind(v) != NodeKind::Function,
        };
        if whole { self.ix.subtree(v) } else { v..v + 1 }
    }

    /// Direct callers of `v`: (caller, best confidence, override only). Zero copy over the
    /// reverse CSR row, callers ascending.
    pub fn direct_callers(&self, v: NodeId) -> impl Iterator<Item = (NodeId, Confidence, bool)> + '_ {
        neighbours(self.graph.reverse(), v)
    }

    /// Direct callees of `v`, as [`Query::direct_callers`].
    pub fn direct_callees(&self, v: NodeId) -> impl Iterator<Item = (NodeId, Confidence, bool)> + '_ {
        neighbours(self.graph.forward(), v)
    }

    /// The call sites of `caller` that call `callee` (override edges: one site at the
    /// override, see [`Site`]). Scans the call edges of the caller's file.
    pub fn call_sites(&self, caller: NodeId, callee: NodeId) -> impl Iterator<Item = Site> + '_ {
        let ix = self.ix;
        let fc = ix.file(caller);
        let fd = ix.file(callee).filter(|&f| Some(f) != fc);
        let own = fc.map_or(&[][..], |f| self.calls.file_edges(f));
        // Override edges live in the overriding class's file.
        let other = fd.map_or(&[][..], |f| self.calls.file_edges(f));
        own.iter()
            .chain(other.iter().filter(|e| e.reason == Reason::Override))
            .filter(move |e| e.src == caller && e.dst == callee)
            .map(move |e| Site::of(ix, e.src, e.dst, e.site, e.conf, e.reason))
    }
}

/// Neighbours of `v` in `csr` with their decoded labels.
fn neighbours(csr: &Csr, v: NodeId) -> impl Iterator<Item = (NodeId, Confidence, bool)> + '_ {
    csr.neighbours(v)
        .iter()
        .zip(csr.neighbour_labels(v))
        .map(|(&w, &l)| (w, label_conf(l), l & LABEL_OVERRIDE != 0))
}

/// One call site behind a call edge.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Site {
    /// File of the site.
    pub file: FileId,
    /// 1-based line and 1-based column of the callee expression (a decorator: the decorated
    /// def's first line, column 1; an override: the overriding def's name).
    pub line: u32,
    pub col: u32,
    pub conf: Confidence,
    pub reason: Reason,
}

impl Site {
    fn of(ix: &Index, src: NodeId, dst: NodeId, site: u32, conf: Confidence, reason: Reason) -> Site {
        let st = ix.facts();
        let (file, line, col) = match reason {
            Reason::Override => {
                let r = ix.def_row(dst).unwrap_or(0);
                (ix.file(dst).unwrap_or(0), st.defs.name_line[r], st.defs.name_col[r] + 1)
            }
            Reason::Decorator => {
                let f = ix.file(src).unwrap_or(0);
                let owner = st.decorators.owner[site as usize];
                let r = (ix.file_facts(f).defs.start + owner) as usize;
                (f, st.defs.start_line[r], 1)
            }
            _ => (ix.file(src).unwrap_or(0), st.calls.line[site as usize], st.calls.col[site as usize] + 1),
        };
        Site { file, line, col, conf, reason }
    }
}

/// A borrowed view of one node: every accessor reads the index, nothing is copied.
#[derive(Clone, Copy)]
pub struct SymbolRef<'a> {
    ix: &'a Index,
    node: NodeId,
}

impl<'a> SymbolRef<'a> {
    #[inline]
    pub fn node(&self) -> NodeId {
        self.node
    }

    #[inline]
    pub fn kind(&self) -> NodeKind {
        self.ix.kind(self.node)
    }

    /// Simple name (last segment).
    #[inline]
    pub fn name(&self) -> &'a [u8] {
        self.ix.name(self.node)
    }

    /// Appends the qualified id to `out`.
    #[inline]
    pub fn qid(&self, out: &mut Vec<u8>) {
        self.ix.qid(self.node, out);
    }

    #[inline]
    pub fn file(&self) -> Option<FileId> {
        self.ix.file(self.node)
    }

    /// Path of the defining file (`None` for distributions and namespace packages).
    #[inline]
    pub fn path(&self) -> Option<&'a Path> {
        self.file().map(|f| self.ix.file_path(f))
    }

    /// Origin of the node's group.
    pub fn origin(&self) -> Origin {
        let d = self.ix.ancestor_at(self.node, 0);
        let g = self.ix.groups();
        // Distribution nodes are in group order (sorted by origin, then name).
        match self.file() {
            Some(f) => self.ix.file_origin(f),
            None => (0..g.len() as u32).find(|&i| self.ix.group_node(i) == d).map_or(Origin::FirstParty, |i| g[i as usize].origin),
        }
    }

    /// 1-based line of the def's name (1 for packages and modules, 0 without a file).
    pub fn line(&self) -> u32 {
        match (self.kind().is_def(), self.ix.def_row(self.node)) {
            (true, Some(r)) => self.ix.facts().defs.name_line[r],
            (false, Some(_)) => 1,
            _ => 0,
        }
    }

    /// 1-based column of the def's name (1 for packages and modules, 0 without a file).
    pub fn col(&self) -> u32 {
        match (self.kind().is_def(), self.ix.def_row(self.node)) {
            (true, Some(r)) => self.ix.facts().defs.name_col[r] + 1,
            (false, Some(_)) => 1,
            _ => 0,
        }
    }
}

/// Visit marks of one traversal side: `mark[v] == stamp` means visited in the current query,
/// and `pos[v]` is then the node's row in the query's output.
#[derive(Default, Debug)]
struct Marks {
    stamp: u32,
    mark: Vec<u32>,
    pos: Vec<u32>,
}

impl Marks {
    /// Starts a query over ids `0..n`: a new generation, arrays grown (zeroed) to `n`.
    fn begin(&mut self, n: usize) {
        if self.mark.len() < n {
            self.mark.resize(n, 0);
            self.pos.resize(n, 0);
        }
        if self.stamp == u32::MAX {
            self.mark.fill(0);
            self.stamp = 0;
        }
        self.stamp += 1;
    }

    #[inline]
    fn get(&self, v: u32) -> Option<u32> {
        (self.mark[v as usize] == self.stamp).then(|| self.pos[v as usize])
    }

    #[inline]
    fn seen(&self, v: u32) -> bool {
        self.mark[v as usize] == self.stamp
    }

    #[inline]
    fn set(&mut self, v: u32, pos: u32) {
        self.mark[v as usize] = self.stamp;
        self.pos[v as usize] = pos;
    }

    fn heap_bytes(&self) -> usize {
        (self.mark.capacity() + self.pos.capacity()) * 4
    }
}

/// Reusable traversal buffers (see the module docs). `Default` allocates nothing.
#[derive(Default, Debug)]
pub struct Scratch {
    a: Marks,
    b: Marks,
    queue: Vec<u32>,
    buf: Vec<u32>,
}

impl Scratch {
    /// Heap bytes (capacity).
    pub fn heap_bytes(&self) -> usize {
        self.a.heap_bytes() + self.b.heap_bytes() + (self.queue.capacity() + self.buf.capacity()) * 4
    }
}
