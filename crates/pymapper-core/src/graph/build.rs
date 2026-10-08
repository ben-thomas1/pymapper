//! Graphs of an index: the call graph over nodes, and module-level import and call graphs.

use super::csr::Graph;
use crate::index::{FileId, Index, NONE, NodeId, NodeKind};
use crate::resolve::{Confidence, Imports, Reason, ResolvedCalls};

/// Which call edges enter a call graph.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CallFilter {
    /// Least sure confidence kept (`Possible` keeps every edge with a target).
    pub threshold: Confidence,
    /// Bit `1 << reason` set: edges with that [`Reason`] are dropped.
    pub exclude: u64,
}

impl CallFilter {
    /// Every edge at least as sure as `threshold`.
    pub fn at_least(threshold: Confidence) -> CallFilter {
        CallFilter { threshold, exclude: 0 }
    }

    /// Also drops edges with reason `r`.
    pub fn without(self, r: Reason) -> CallFilter {
        CallFilter { exclude: self.exclude | (1 << r as u8), ..self }
    }

    #[inline]
    pub fn keeps(&self, conf: Confidence, reason: Reason) -> bool {
        conf <= self.threshold && conf != Confidence::Unresolved && self.exclude & (1 << reason as u8) == 0
    }
}

impl Default for CallFilter {
    /// Every edge with a target.
    fn default() -> Self {
        CallFilter::at_least(Confidence::Possible)
    }
}

/// The call graph over every node of `ix` (node ids are [`NodeId`]s): an edge `src -> dst`
/// for every [`CallEdge`](crate::resolve::CallEdge) the filter keeps, parallel edges merged.
/// The label of an edge is the best [`Confidence`] (as `u8`) of the call edges behind it.
pub fn call_graph(ix: &Index, calls: &ResolvedCalls, filter: CallFilter) -> Graph {
    let edges = calls.edges();
    Graph::build_labelled(ix.node_count(), || {
        edges.iter().filter(move |e| filter.keeps(e.conf, e.reason)).map(|e| (e.src, e.dst, e.conf as u8))
    })
}

/// A graph over the package and module nodes of an index (namespace packages included), with
/// dense local ids `0..len` in [`NodeId`] order.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ModuleGraph {
    /// Index node of every local id, ascending.
    nodes: Vec<NodeId>,
    graph: Graph,
}

impl ModuleGraph {
    /// The graph over local ids.
    #[inline]
    pub fn graph(&self) -> &Graph {
        &self.graph
    }

    /// Index node of local id `m`.
    #[inline]
    pub fn node(&self, m: u32) -> NodeId {
        self.nodes[m as usize]
    }

    /// Index node of every local id, ascending.
    #[inline]
    pub fn nodes(&self) -> &[NodeId] {
        &self.nodes
    }

    /// Local id of package/module node `v` (binary search).
    #[inline]
    pub fn local(&self, v: NodeId) -> Option<u32> {
        self.nodes.binary_search(&v).ok().map(|i| i as u32)
    }

    /// Number of modules.
    #[inline]
    pub fn len(&self) -> u32 {
        self.nodes.len() as u32
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// Heap bytes (capacity).
    pub fn heap_bytes(&self) -> usize {
        self.nodes.capacity() * 4 + self.graph.heap_bytes()
    }
}

/// Package and module nodes of `ix`, ascending. Skips def subtrees (a module's subtree holds
/// only defs), so this costs O(packages + modules + defs of `__init__` files).
fn module_nodes(ix: &Index) -> Vec<NodeId> {
    let mut out = Vec::new();
    let n = ix.node_count();
    let mut v = 0;
    while v < n {
        match ix.kind(v) {
            NodeKind::Distribution => v += 1,
            NodeKind::Package => {
                out.push(v);
                v += 1;
            }
            NodeKind::Module => {
                out.push(v);
                v = ix.subtree(v).end;
            }
            NodeKind::Class | NodeKind::Function => v = ix.subtree(v).end,
        }
    }
    out
}

/// Local id of every file's package/module node (`NONE` for files without one).
fn file_locals(ix: &Index, nodes: &[NodeId]) -> Vec<u32> {
    let mut out = Vec::with_capacity(ix.file_count() as usize);
    for f in 0..ix.file_count() {
        let v = ix.file_node(f);
        let m = if v == NONE { NONE } else { nodes.binary_search(&v).map_or(NONE, |i| i as u32) };
        out.push(m);
    }
    out
}

/// Which module edges enter an import graph: an edge is dropped when its flags (the
/// [`res_flags`](crate::resolve::res_flags) shared by every import row behind it) intersect
/// `exclude`. `exclude = TYPE_ONLY | LAZY` gives the graph of what runs at import time.
pub fn import_graph(ix: &Index, imp: &Imports, exclude: u8) -> ModuleGraph {
    let nodes = module_nodes(ix);
    let locals = file_locals(ix, &nodes);
    // Destination local ids: binary search per edge (module edges are few compared to calls).
    let nodes_ref = &nodes;
    let locals = &locals;
    let local = move |v: NodeId| nodes_ref.binary_search(&v).map_or(NONE, |i| i as u32);
    let graph = Graph::build_labelled(nodes.len() as u32, || {
        imp.all_module_edges().filter_map(move |(f, e)| {
            let s = locals[f as usize];
            let d = local(e.dst);
            (s != NONE && d != NONE && e.flags & exclude == 0).then_some((s, d, e.flags))
        })
    });
    ModuleGraph { nodes, graph }
}

/// The module-level call graph: an edge between the modules of the caller and the callee for
/// every call edge the filter keeps, calls within one module dropped. Labels as
/// [`call_graph`]: the best confidence behind the edge.
pub fn module_call_graph(ix: &Index, calls: &ResolvedCalls, filter: CallFilter) -> ModuleGraph {
    let nodes = module_nodes(ix);
    let locals = file_locals(ix, &nodes);
    let locals = &locals;
    let module_of = move |v: NodeId| ix.file(v).map_or(NONE, |f: FileId| locals[f as usize]);
    let edges = calls.edges();
    let graph = Graph::build_labelled(nodes.len() as u32, || {
        edges.iter().filter_map(move |e| {
            if !filter.keeps(e.conf, e.reason) {
                return None;
            }
            let s = module_of(e.src);
            let d = module_of(e.dst);
            (s != NONE && d != NONE && s != d).then_some((s, d, e.conf as u8))
        })
    });
    ModuleGraph { nodes, graph }
}
