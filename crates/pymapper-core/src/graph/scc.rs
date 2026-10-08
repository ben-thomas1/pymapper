//! Strongly connected components: iterative Tarjan in Pearce's space-efficient form.

use super::csr::{Csr, Graph};

/// Strongly connected components of a [`Graph`]. Component ids are a topological order of the
/// condensation: for every edge `u -> v` with `comp(u) != comp(v)`, `comp(u) < comp(v)`
/// (sources first; iterate ids downwards for reverse topological order, sinks first).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Scc {
    /// Component of every node.
    comp: Vec<u32>,
    /// Members of component `c`: `members[start[c]..start[c + 1]]`, ascending node ids.
    start: Vec<u32>,
    members: Vec<u32>,
}

impl Scc {
    /// Computes the components of `g` (see [`tarjan`]).
    pub fn of(g: &Graph) -> Scc {
        let (comp, k) = tarjan_count(g.forward());
        Scc::from_comp(comp, k as usize)
    }

    /// Groups nodes by component (counting sort, members ascending); `k` components.
    fn from_comp(comp: Vec<u32>, k: usize) -> Scc {
        let mut start = vec![0u32; k + 1];
        for &c in &comp {
            start[c as usize + 1] += 1;
        }
        for i in 0..k {
            start[i + 1] += start[i];
        }
        let mut members = vec![0u32; comp.len()];
        for (v, &c) in comp.iter().enumerate() {
            members[start[c as usize] as usize] = v as u32;
            start[c as usize] += 1;
        }
        for i in (1..=k).rev() {
            start[i] = start[i - 1];
        }
        start[0] = 0;
        Scc { comp, start, members }
    }

    /// Number of components.
    #[inline]
    pub fn count(&self) -> u32 {
        (self.start.len().max(1) - 1) as u32
    }

    /// Component of node `v`.
    #[inline]
    pub fn comp(&self, v: u32) -> u32 {
        self.comp[v as usize]
    }

    /// Component of every node.
    #[inline]
    pub fn comps(&self) -> &[u32] {
        &self.comp
    }

    /// Members of component `c`, ascending.
    #[inline]
    pub fn members(&self, c: u32) -> &[u32] {
        &self.members[self.start[c as usize] as usize..self.start[c as usize + 1] as usize]
    }

    /// Size of component `c`.
    #[inline]
    pub fn size(&self, c: u32) -> u32 {
        self.start[c as usize + 1] - self.start[c as usize]
    }

    /// Whether component `c` contains a cycle of `g`: two or more members, or one member with
    /// a self loop.
    #[inline]
    pub fn is_cyclic(&self, g: &Graph, c: u32) -> bool {
        match self.members(c) {
            [v] => g.has_edge(*v, *v),
            m => m.len() > 1,
        }
    }

    /// Components with two or more members, in id order.
    pub fn nontrivial(&self) -> impl Iterator<Item = u32> + '_ {
        (0..self.count()).filter(|&c| self.size(c) > 1)
    }

    /// Heap bytes (capacity).
    pub fn heap_bytes(&self) -> usize {
        (self.comp.capacity() + self.start.capacity() + self.members.capacity()) * 4
    }
}

/// Component of every node of the graph `fwd`, ids topologically ordered (see [`Scc`]).
///
/// Iterative (explicit stack, no recursion: safe on million-node chains) Tarjan in the
/// formulation of Pearce ("A space-efficient algorithm for finding strongly connected
/// components", IPL 2016): one `u32` per node (`rindex`, later overwritten with the component)
/// and one root bit per node instead of Tarjan's index + lowlink + on-stack flag. Extra
/// memory: `n / 8` bytes of root bits, the DFS stack (`8` bytes per frame) and the component
/// stack (`4` bytes per node on it). O(n + m) time.
///
/// `rindex[v]` is `0` while unvisited, the DFS index (`1, 2, ...`, reused after components
/// complete) while active, and once `v`'s component completes `n - k` for the `k`-th
/// completed component (0-based). Completed values are always larger than every active one, so
/// edges into completed components never lower a node's `rindex` (no on-stack flag needed).
pub fn tarjan(fwd: &Csr) -> Vec<u32> {
    tarjan_count(fwd).0
}

/// [`tarjan`] plus the number of components.
fn tarjan_count(fwd: &Csr) -> (Vec<u32>, u32) {
    let n = fwd.node_count();
    assert!(n < u32::MAX, "too many nodes");
    let adj = fwd.targets();
    let off = fwd.offsets();
    let mut rindex = vec![0u32; n as usize];
    let mut root = vec![0u64; (n as usize).div_ceil(64)];
    // DFS frames: (node, next edge position).
    let mut dfs: Vec<(u32, u32)> = Vec::new();
    // Tarjan's component stack (non-root nodes whose component is open).
    let mut stack: Vec<u32> = Vec::new();
    let mut index = 1u32;
    let mut c = n;
    for s in 0..n {
        if rindex[s as usize] != 0 {
            continue;
        }
        rindex[s as usize] = index;
        index += 1;
        root[s as usize >> 6] |= 1 << (s & 63);
        dfs.push((s, off[s as usize]));
        while let Some(&(vv, pos)) = dfs.last() {
            let v = vv as usize;
            let end = off[v + 1];
            let mut i = pos;
            let mut descended = false;
            while i < end {
                let w = adj[i as usize] as usize;
                if rindex[w] == 0 {
                    // Resume at the same edge: after `w` completes it is handled as visited.
                    let last = dfs.len() - 1;
                    dfs[last].1 = i;
                    rindex[w] = index;
                    index += 1;
                    root[w >> 6] |= 1 << (w & 63);
                    dfs.push((w as u32, off[w]));
                    descended = true;
                    break;
                }
                if rindex[w] < rindex[v] {
                    rindex[v] = rindex[w];
                    root[v >> 6] &= !(1 << (v & 63));
                }
                i += 1;
            }
            if descended {
                continue;
            }
            dfs.pop();
            if root[v >> 6] & (1 << (v & 63)) != 0 {
                index -= 1;
                while let Some(&w) = stack.last() {
                    if rindex[w as usize] < rindex[v] {
                        break;
                    }
                    stack.pop();
                    rindex[w as usize] = c;
                    index -= 1;
                }
                rindex[v] = c;
                c -= 1;
            } else {
                stack.push(v as u32);
            }
        }
    }
    // Component k (0-based completion order) holds n - k; completion order is reverse
    // topological, so id = (n - k) - (c + 1) = count - 1 - k puts sources first.
    for r in &mut rindex {
        *r -= c + 1;
    }
    (rindex, n - c)
}
