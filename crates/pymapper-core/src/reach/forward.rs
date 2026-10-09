//! Forward reachability from a root set: breadth-first over the CSR, one bit per node.

use super::bits::BitSet;
use crate::graph::Graph;
use crate::index::NONE;

/// The nodes reachable from a set of roots, with a shortest-path predecessor per node (the
/// "why is this live" chain back to a root).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Reached {
    live: BitSet,
    /// BFS predecessor of every reached non-root node; `NONE` for roots and unreached nodes.
    via: Vec<u32>,
}

impl Reached {
    /// Whether `v` is reachable from a root (roots included).
    #[inline]
    pub fn contains(&self, v: u32) -> bool {
        self.live.contains(v)
    }

    /// The reached nodes.
    #[inline]
    pub fn live(&self) -> &BitSet {
        &self.live
    }

    /// Number of reached nodes.
    pub fn count(&self) -> u32 {
        self.live.count()
    }

    /// The node `v` was first reached from (`None` for roots and unreached nodes).
    #[inline]
    pub fn via(&self, v: u32) -> Option<u32> {
        let p = self.via[v as usize];
        (p != NONE).then_some(p)
    }

    /// A shortest path from a root to `v`, root first (empty if `v` is unreached). O(length).
    pub fn path_to(&self, v: u32) -> Vec<u32> {
        if !self.contains(v) {
            return Vec::new();
        }
        let mut out = vec![v];
        let mut cur = v;
        while let Some(p) = self.via(cur) {
            out.push(p);
            cur = p;
        }
        out.reverse();
        out
    }

    /// Heap bytes (capacity).
    pub fn heap_bytes(&self) -> usize {
        self.live.heap_bytes() + self.via.capacity() * 4
    }
}

/// Every node reachable from `roots` in `g` (roots included). Breadth first, so
/// [`Reached::path_to`] gives shortest paths. O(V + E) time; memory: one bit and one `u32`
/// (predecessor) per node plus the queue (one `u32` per reached node).
///
/// # Panics
/// If a root is `>= g.node_count()`.
pub fn forward(g: &Graph, roots: impl IntoIterator<Item = u32>) -> Reached {
    let n = g.node_count();
    let fwd = g.forward();
    let off = fwd.offsets();
    let adj = fwd.targets();
    let mut live = BitSet::new(n);
    let mut via = vec![NONE; n as usize];
    let mut queue: Vec<u32> = Vec::new();
    for r in roots {
        if live.insert_new(r) {
            queue.push(r);
        }
    }
    let mut head = 0;
    while head < queue.len() {
        let v = queue[head];
        head += 1;
        for &w in &adj[off[v as usize] as usize..off[v as usize + 1] as usize] {
            if live.insert_new(w) {
                via[w as usize] = v;
                queue.push(w);
            }
        }
    }
    Reached { live, via }
}
