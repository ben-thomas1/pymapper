//! Transitive contracts: which contract module each unlisted module reaches through unlisted
//! modules only. Reverse multi-source BFS over the import graph, every module and edge visited
//! a bounded number of times: O(V + E) per contract (per container for layers).

use super::compile::{Compiled, Kind, NO_ROLE};
use crate::graph::Graph;
use crate::index::NONE;

/// Per unlisted module (no role in the contract): the listed module it reaches (local id), or
/// for `forbidden` the raw `Sym` of the forbidden name. `NONE` when it reaches none.
pub(super) enum Reach {
    /// Per container `g`: `hit[g * n + v]`, the highest-layer module of `g` reached (lowest
    /// layer index; ties: first found).
    Layers { hit: Vec<u32>, n: usize },
    /// The first two modules of distinct `modules` entries reached.
    Independence { hit: Vec<[u32; 2]> },
    /// Name of a forbidden module reached.
    Forbidden { name: Vec<u32> },
}

impl Reach {
    /// Highest-layer module of container `g` reached from `v`, if any.
    #[inline]
    pub fn layer_hit(&self, g: u32, v: u32) -> u32 {
        match self {
            Reach::Layers { hit, n } => hit[g as usize * n + v as usize],
            _ => NONE,
        }
    }

    /// A module of another `modules` entry than `member` reached from `v`, if any.
    #[inline]
    pub fn independence_hit(&self, c: &Compiled, v: u32, member: u32) -> u32 {
        match self {
            Reach::Independence { hit } => {
                for &h in &hit[v as usize] {
                    if h != NONE && c.slots[c.src[h as usize] as usize].member != member {
                        return h;
                    }
                }
                NONE
            }
            _ => NONE,
        }
    }

    /// Name (raw `Sym`) of a forbidden module reached from `v`, if any.
    #[inline]
    pub fn forbidden_hit(&self, v: u32) -> u32 {
        match self {
            Reach::Forbidden { name } => name[v as usize],
            _ => NONE,
        }
    }

    pub fn heap_bytes(&self) -> usize {
        match self {
            Reach::Layers { hit, .. } => hit.capacity() * 4,
            Reach::Independence { hit } => hit.capacity() * 8,
            Reach::Forbidden { name } => name.capacity() * 4,
        }
    }
}

/// Whether local `v` is unlisted in `c` (an allowed intermediate of a transitive chain).
#[inline]
fn unlisted(c: &Compiled, v: u32) -> bool {
    c.src[v as usize] == NO_ROLE && (c.dst.is_empty() || c.dst[v as usize] == NO_ROLE)
}

/// Layers: per container, layers in order from the highest, a reverse BFS from the layer's
/// modules into unlisted modules not reached yet. A module reached from layer `l` first can
/// not reach a higher layer (it would have been reached from it), so the first hit is the
/// highest; every unlisted module is visited once per container.
pub(super) fn layers(c: &Compiled, g: &Graph) -> Reach {
    let Kind::Layers { groups, layers, .. } = c.kind else { unreachable!("layers contract") };
    let n = c.src.len();
    let mut hit = vec![NONE; n * groups as usize];
    // Listed modules by (container, layer): counting sort over the slot buckets.
    let buckets = (groups * layers) as usize;
    let mut start = vec![0u32; buckets + 1];
    let bucket = |v: usize| {
        let s = c.slots[c.src[v] as usize];
        (s.group * layers + s.layer) as usize
    };
    for v in 0..n {
        if c.src[v] != NO_ROLE {
            start[bucket(v) + 1] += 1;
        }
    }
    for i in 0..buckets {
        start[i + 1] += start[i];
    }
    let mut order = vec![0u32; start[buckets] as usize];
    let mut cur = start.clone();
    for v in 0..n {
        if c.src[v] != NO_ROLE {
            let b = bucket(v);
            order[cur[b] as usize] = v as u32;
            cur[b] += 1;
        }
    }
    let mut queue: Vec<u32> = Vec::new();
    for grp in 0..groups as usize {
        let hit = &mut hit[grp * n..(grp + 1) * n];
        for l in 0..layers as usize {
            let b = grp * layers as usize + l;
            queue.clear();
            queue.extend_from_slice(&order[start[b] as usize..start[b + 1] as usize]);
            let mut head = 0;
            while head < queue.len() {
                let u = queue[head];
                head += 1;
                let origin = if c.src[u as usize] != NO_ROLE { u } else { hit[u as usize] };
                for &p in g.pred(u) {
                    if hit[p as usize] == NONE && unlisted(c, p) {
                        hit[p as usize] = origin;
                        queue.push(p);
                    }
                }
            }
        }
    }
    Reach::Layers { hit, n }
}

/// Independence: one reverse BFS from every listed module; an unlisted module keeps the first
/// two hits of distinct `modules` entries (enough to find one other than the importer's), so
/// it is queued at most twice.
pub(super) fn independence(c: &Compiled, g: &Graph) -> Reach {
    let n = c.src.len();
    let mut hit = vec![[NONE; 2]; n];
    let member = |v: u32| c.slots[c.src[v as usize] as usize].member;
    // (module, listed module reached).
    let mut queue: Vec<(u32, u32)> = Vec::new();
    for v in 0..n as u32 {
        if c.src[v as usize] != NO_ROLE {
            queue.push((v, v));
        }
    }
    let mut head = 0;
    while head < queue.len() {
        let (u, origin) = queue[head];
        head += 1;
        let m = member(origin);
        for &p in g.pred(u) {
            if !unlisted(c, p) {
                continue;
            }
            let h = &mut hit[p as usize];
            if h[0] == NONE {
                h[0] = origin;
            } else if h[1] == NONE && member(h[0]) != m {
                h[1] = origin;
            } else {
                continue;
            }
            queue.push((p, origin));
        }
    }
    Reach::Independence { hit }
}

/// Forbidden: a reverse BFS from the loaded forbidden modules and from the unlisted modules
/// that import a forbidden module which is not loaded (`seeds`: module, name), into unlisted
/// modules. `name_of` names a loaded forbidden module.
pub(super) fn forbidden(c: &Compiled, g: &Graph, seeds: &[(u32, u32)], name_of: impl Fn(u32) -> u32) -> Reach {
    let n = c.src.len();
    let mut name = vec![NONE; n];
    let mut queue: Vec<(u32, u32)> = Vec::new();
    for v in 0..n as u32 {
        if c.dst[v as usize] != NO_ROLE {
            queue.push((v, name_of(v)));
        }
    }
    for &(v, s) in seeds {
        if unlisted(c, v) && name[v as usize] == NONE {
            name[v as usize] = s;
            queue.push((v, s));
        }
    }
    let mut head = 0;
    while head < queue.len() {
        let (u, s) = queue[head];
        head += 1;
        for &p in g.pred(u) {
            if name[p as usize] == NONE && unlisted(c, p) {
                name[p as usize] = s;
                queue.push((p, s));
            }
        }
    }
    Reach::Forbidden { name }
}
