//! Exact reach sizes: one depth-first search over the condensed DAG per component.

use rayon::prelude::*;

use super::sizes::Weights;
use crate::graph::Condensed;

/// Exact `[symbols, lines]` reachable from every component (itself included), saturating at
/// `u32::MAX`. Components are split into chunks searched in parallel; each chunk owns one
/// visit stamp per component (`4 k` bytes) and a stack, so no clearing between searches.
pub(super) fn component_sizes(cd: &Condensed, w: &Weights, out: &mut [[u32; 2]]) {
    let k = cd.count() as usize;
    if k == 0 {
        return;
    }
    let scc = cd.scc();
    // Per-component weights.
    let mut own = vec![[0u64; 2]; k];
    for (v, &c) in scc.comps().iter().enumerate() {
        let o = &mut own[c as usize];
        o[0] += u64::from(w.symbol.contains(v as u32));
        o[1] += u64::from(w.lines[v]);
    }
    let dag = cd.dag().forward();
    let off = dag.offsets();
    let adj = dag.targets();
    let chunks = (rayon::current_num_threads() * 4).max(1);
    let chunk = k.div_ceil(chunks);
    let own = &own;
    out.par_chunks_mut(chunk).enumerate().for_each(|(i, slice)| {
        let first = (i * chunk) as u32;
        let mut stamp = vec![0u32; k];
        let mut stack: Vec<u32> = Vec::new();
        for (j, o) in slice.iter_mut().enumerate() {
            let c = first + j as u32;
            // Stamps are `c + 1`: unique per search, so `stamp` is never cleared.
            let mark = c + 1;
            stamp[c as usize] = mark;
            stack.push(c);
            let mut sum = [0u64; 2];
            while let Some(x) = stack.pop() {
                let ow = own[x as usize];
                sum[0] += ow[0];
                sum[1] += ow[1];
                for &y in &adj[off[x as usize] as usize..off[x as usize + 1] as usize] {
                    if stamp[y as usize] != mark {
                        stamp[y as usize] = mark;
                        stack.push(y);
                    }
                }
            }
            *o = [sum[0].min(u64::from(u32::MAX)) as u32, sum[1].min(u64::from(u32::MAX)) as u32];
        }
    });
}
