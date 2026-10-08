//! CSR graphs, iterative Tarjan SCC, condensed DAG (design: Semantics). Owned by the graph step.
//!
//! ```text
//!   edge list (iterated twice) ──> Graph      forward + reverse CSR over 0..n, u32 offsets
//!                                  │          and targets, rows ascending, parallel edges
//!                                  │          merged (min u8 label), self loops kept
//!                                  ├─> Scc         iterative Tarjan (Pearce, no recursion):
//!                                  │               comp per node, members per comp; comp
//!                                  │               ids are a topological order
//!                                  └─> Condensed   Scc + Graph over comp ids (cross edges)
//!
//!   Index + ResolvedCalls ──> call_graph         nodes = NodeIds, label = best Confidence
//!                        └──> module_call_graph  nodes = packages/modules (ModuleGraph)
//!   Index + Imports ───────> import_graph        nodes = packages/modules, label = edge flags
//! ```
//!
//! # Memory
//!
//! A [`Graph`] with `n` nodes and `m` distinct edges holds `8 (n + 1) + 8 m` bytes, plus
//! `2 m` when labelled. Building it costs two passes over the edge source (count, scatter) and
//! needs no temporary edge list: rows are scattered unsorted into the forward buffers, the
//! transpose sorts them by source (counting sort), duplicates are merged there, and the
//! transpose of that is the sorted forward direction, written into the scatter buffers. Before
//! deduplication both directions hold every input edge. O(n + m), no comparison sort.
//!
//! [`tarjan`] keeps one `u32` per node (overwritten in place with the component id), one bit
//! per node and two stacks. [`Scc`] adds the members grouping (`4 (n + k)` bytes).
//!
//! # Topological order
//!
//! Tarjan completes components sinks first; ids are assigned so that the first completed
//! component gets the highest id. Every condensed edge therefore goes from a lower to a higher
//! component id: [`Condensed::topo`] is `0..k` and [`Condensed::reverse_topo`] is `(0..k).rev()`
//! (what reachability needs: successors before predecessors), with no order array.
//!
//! # Determinism
//!
//! Node ids come from the index (deterministic), CSR rows are sorted, and Tarjan visits nodes
//! and edges in id order: component ids depend only on the graph.
//!
//! # Index graphs
//!
//! - [`call_graph`]: every [`CallEdge`](crate::resolve::CallEdge) at least as sure as the
//!   [`CallFilter`] threshold, minus excluded reasons. Override edges (base method to
//!   override, `Likely`) are included unless excluded: they keep overrides alive under dynamic
//!   dispatch.
//! - [`import_graph`]: the module edges of [`Imports`](crate::resolve::Imports) whose flags do
//!   not intersect a mask (for instance `TYPE_ONLY | LAZY` for what runs at import time).
//! - [`module_call_graph`]: call edges lifted to the modules of caller and callee, calls
//!   within one module dropped.
//!
//! Module graphs use dense local ids over the package/module nodes (namespace packages
//! included) in node order; [`ModuleGraph::local`] maps back by binary search.

mod build;
mod csr;
mod dag;
mod scc;

#[cfg(test)]
mod index_tests;
#[cfg(test)]
mod tests;

pub use build::{CallFilter, ModuleGraph, call_graph, import_graph, module_call_graph};
pub use csr::{Csr, Graph};
pub use dag::Condensed;
pub use scc::{Scc, tarjan};
