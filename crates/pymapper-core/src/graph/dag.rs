//! The condensation of a graph: one node per strongly connected component.

use std::iter::Rev;
use std::ops::Range;

use super::csr::Graph;
use super::scc::Scc;

/// The condensed DAG of a graph: its [`Scc`] plus a [`Graph`] over component ids with one edge
/// per pair of distinct components joined by at least one edge (labels: the minimum label of
/// the edges behind it, if the source graph is labelled). Component ids are a topological
/// order, so the DAG needs no separate order array: [`Condensed::topo`] is `0..count`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Condensed {
    scc: Scc,
    dag: Graph,
}

impl Condensed {
    /// Condenses `g`: SCCs, then the edges between components. O(n + m).
    pub fn of(g: &Graph) -> Condensed {
        let scc = Scc::of(g);
        let dag = condense(g, &scc);
        Condensed { scc, dag }
    }

    /// The components.
    #[inline]
    pub fn scc(&self) -> &Scc {
        &self.scc
    }

    /// The DAG over component ids (forward and reverse).
    #[inline]
    pub fn dag(&self) -> &Graph {
        &self.dag
    }

    /// Number of components (DAG nodes).
    #[inline]
    pub fn count(&self) -> u32 {
        self.scc.count()
    }

    /// Component ids in topological order (every DAG edge goes from a lower to a higher id).
    #[inline]
    pub fn topo(&self) -> Range<u32> {
        0..self.count()
    }

    /// Component ids in reverse topological order (sinks first): the order for bottom-up
    /// propagation such as reachability sketches.
    #[inline]
    pub fn reverse_topo(&self) -> Rev<Range<u32>> {
        self.topo().rev()
    }

    /// Heap bytes (capacity).
    pub fn heap_bytes(&self) -> usize {
        self.scc.heap_bytes() + self.dag.heap_bytes()
    }
}

/// Edges of `g` between distinct components of `scc`, as a graph over component ids.
fn condense(g: &Graph, scc: &Scc) -> Graph {
    let fwd = g.forward();
    let comp = scc.comps();
    let labels = fwd.labels();
    let n = g.node_count();
    let k = scc.count();
    let cross = move || {
        (0..n).flat_map(move |v| {
            let cv = comp[v as usize];
            fwd.range(v).filter_map(move |i| {
                let cw = comp[fwd.targets()[i] as usize];
                (cw != cv).then(|| (cv, cw, labels.get(i).copied().unwrap_or(0)))
            })
        })
    };
    if g.is_labelled() { Graph::build_labelled(k, cross) } else { Graph::build(k, move || cross().map(|(a, b, _)| (a, b))) }
}
