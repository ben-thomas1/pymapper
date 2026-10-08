use super::*;

/// xorshift64* (deterministic test graphs without dependencies).
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    fn below(&mut self, n: u32) -> u32 {
        (self.next() % u64::from(n)) as u32
    }
}

fn random_edges(rng: &mut Rng, n: u32, m: usize) -> Vec<(u32, u32)> {
    (0..m).map(|_| (rng.below(n), rng.below(n))).collect()
}

/// Checks the CSR invariants of `g` against the edge set `edges`.
fn check_graph(g: &Graph, n: u32, edges: &[(u32, u32)]) {
    let mut want: Vec<(u32, u32)> = edges.to_vec();
    want.sort_unstable();
    want.dedup();
    assert_eq!(g.node_count(), n);
    assert_eq!(g.edge_count() as usize, want.len());
    let mut got = Vec::new();
    for v in 0..n {
        let s = g.succ(v);
        assert!(s.windows(2).all(|w| w[0] < w[1]), "row {v} not strictly ascending");
        got.extend(s.iter().map(|&w| (v, w)));
        let p = g.pred(v);
        assert!(p.windows(2).all(|w| w[0] < w[1]), "reverse row {v} not strictly ascending");
        for &u in p {
            assert!(g.has_edge(u, v));
        }
    }
    assert_eq!(got, want);
    assert_eq!(g.reverse().edge_count(), g.edge_count());
}

#[test]
fn csr_basic() {
    let edges = [(0, 1), (0, 2), (2, 0), (0, 1), (1, 1), (3, 2), (2, 0)];
    let g = Graph::from_edges(5, &edges);
    check_graph(&g, 5, &edges);
    assert_eq!(g.succ(0), &[1, 2]);
    assert_eq!(g.succ(1), &[1]);
    assert_eq!(g.succ(4), &[] as &[u32]);
    assert_eq!(g.pred(2), &[0, 3]);
    assert_eq!(g.pred(1), &[0, 1]);
    assert!(!g.is_labelled());
    assert_eq!(g.label(0, 1), None);
    assert_eq!(g.forward().degree(0), 2);
    assert_eq!(g.forward().offsets().len(), 6);
}

#[test]
fn csr_empty() {
    let g = Graph::from_edges(0, &[]);
    assert_eq!(g.node_count(), 0);
    assert_eq!(g.edge_count(), 0);
    let g = Graph::from_edges(3, &[]);
    assert_eq!(g.node_count(), 3);
    assert!(g.succ(2).is_empty() && g.pred(0).is_empty());
    let c = Condensed::of(&g);
    assert_eq!(c.count(), 3);
    assert_eq!(c.dag().edge_count(), 0);
    let c = Condensed::of(&Graph::default());
    assert_eq!(c.count(), 0);
}

#[test]
#[should_panic(expected = "out of range")]
fn csr_rejects_out_of_range() {
    let _ = Graph::from_edges(2, &[(0, 2)]);
}

#[test]
fn csr_labels_keep_minimum() {
    let edges = [(0u32, 1u32, 2u8), (0, 1, 0), (0, 1, 1), (1, 0, 3), (2, 2, 1), (2, 0, 2)];
    let g = Graph::build_labelled(3, || edges.iter().copied());
    assert!(g.is_labelled());
    assert_eq!(g.edge_count(), 4);
    assert_eq!(g.label(0, 1), Some(0));
    assert_eq!(g.label(1, 0), Some(3));
    assert_eq!(g.label(2, 2), Some(1));
    assert_eq!(g.label(2, 1), None);
    assert_eq!(g.forward().neighbour_labels(2), &[2, 1]);
    // Reverse labels agree with forward ones.
    assert_eq!(g.reverse().label(1, 0), Some(0));
    assert_eq!(g.reverse().label(0, 2), Some(2));
    assert_eq!(g.reverse().neighbour_labels(0), &[3, 2]);
}

#[test]
fn csr_random_matches_edge_set() {
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    for round in 0..50 {
        let n = 1 + rng.below(60);
        let m = rng.below(4 * n + 1) as usize;
        let edges = random_edges(&mut rng, n, m);
        let g = Graph::from_edges(n, &edges);
        check_graph(&g, n, &edges);
        assert!(g.heap_bytes() >= (n as usize + 1) * 8, "round {round}");
    }
}

/// Reachability closure by DFS from every node (small graphs only).
fn reach_matrix(g: &Graph) -> Vec<Vec<bool>> {
    let n = g.node_count() as usize;
    let mut out = vec![vec![false; n]; n];
    let mut queue = Vec::new();
    for s in 0..n {
        out[s][s] = true;
        queue.clear();
        queue.push(s as u32);
        while let Some(v) = queue.pop() {
            for &w in g.succ(v) {
                if !out[s][w as usize] {
                    out[s][w as usize] = true;
                    queue.push(w);
                }
            }
        }
    }
    out
}

/// Checks `scc` structurally: dense ids, members consistent, topological cross edges.
fn check_scc_structure(g: &Graph, scc: &Scc) {
    let n = g.node_count();
    let k = scc.count();
    assert_eq!(scc.comps().len(), n as usize);
    let mut total = 0;
    for c in 0..k {
        let m = scc.members(c);
        assert!(!m.is_empty(), "component {c} empty");
        assert!(m.windows(2).all(|w| w[0] < w[1]));
        assert_eq!(scc.size(c) as usize, m.len());
        for &v in m {
            assert_eq!(scc.comp(v), c);
        }
        total += m.len();
    }
    assert_eq!(total, n as usize);
    for v in 0..n {
        for &w in g.succ(v) {
            assert!(scc.comp(v) <= scc.comp(w), "edge {v}->{w} goes against the topological order");
        }
    }
}

/// Checks `scc` against mutual reachability.
fn check_scc_exact(g: &Graph, scc: &Scc) {
    check_scc_structure(g, scc);
    let r = reach_matrix(g);
    let n = g.node_count() as usize;
    for u in 0..n {
        for v in 0..n {
            let same = r[u][v] && r[v][u];
            assert_eq!(scc.comp(u as u32) == scc.comp(v as u32), same, "nodes {u} {v}");
        }
    }
}

#[test]
fn scc_small_known() {
    // 0 <-> 1 -> 2 -> 3 -> 2, 4 alone with a self loop, 5 isolated.
    let g = Graph::from_edges(6, &[(0, 1), (1, 0), (1, 2), (2, 3), (3, 2), (4, 4)]);
    let scc = Scc::of(&g);
    check_scc_exact(&g, &scc);
    assert_eq!(scc.count(), 4);
    let c01 = scc.comp(0);
    assert_eq!(scc.members(c01), &[0, 1]);
    assert_eq!(scc.members(scc.comp(2)), &[2, 3]);
    assert!(scc.comp(0) < scc.comp(2));
    assert!(scc.is_cyclic(&g, c01));
    assert!(scc.is_cyclic(&g, scc.comp(4)));
    assert!(!scc.is_cyclic(&g, scc.comp(5)));
    let nt: Vec<u32> = scc.nontrivial().collect();
    assert_eq!(nt.len(), 2);
}

#[test]
fn scc_ids_are_topological_on_chain() {
    // 4 -> 3 -> 2 -> 1 -> 0: sources first means comp(4) = 0.
    let g = Graph::from_edges(5, &[(4, 3), (3, 2), (2, 1), (1, 0)]);
    let scc = Scc::of(&g);
    check_scc_exact(&g, &scc);
    assert_eq!(scc.comps(), &[4, 3, 2, 1, 0]);
}

#[test]
fn scc_random_exact() {
    let mut rng = Rng(42);
    for _ in 0..300 {
        let n = 1 + rng.below(40);
        let m = rng.below(3 * n + 1) as usize;
        let edges = random_edges(&mut rng, n, m);
        let g = Graph::from_edges(n, &edges);
        check_scc_exact(&g, &Scc::of(&g));
    }
}

#[test]
fn scc_deterministic() {
    let mut rng = Rng(7);
    let edges = random_edges(&mut rng, 500, 1500);
    let mut shuffled = edges.clone();
    shuffled.reverse();
    shuffled.extend_from_slice(&edges[..100]);
    let a = Scc::of(&Graph::from_edges(500, &edges));
    let b = Scc::of(&Graph::from_edges(500, &shuffled));
    assert_eq!(a, b);
}

/// Checks the condensation of `g`: DAG edges are exactly the cross-component edges, go from
/// lower to higher ids, and carry the minimum label.
fn check_condensed(g: &Graph, c: &Condensed) {
    check_scc_structure(g, c.scc());
    let scc = c.scc();
    let mut want: Vec<(u32, u32, u8)> = Vec::new();
    for v in 0..g.node_count() {
        let labels = g.forward().neighbour_labels(v);
        for (i, &w) in g.succ(v).iter().enumerate() {
            let (a, b) = (scc.comp(v), scc.comp(w));
            if a != b {
                want.push((a, b, labels.get(i).copied().unwrap_or(0)));
            }
        }
    }
    want.sort_unstable();
    want.dedup_by(|x, y| x.0 == y.0 && x.1 == y.1);
    let dag = c.dag();
    assert_eq!(dag.node_count(), c.count());
    assert_eq!(dag.is_labelled(), g.is_labelled());
    let mut got = Vec::new();
    for a in c.topo() {
        let labels = dag.forward().neighbour_labels(a);
        for (i, &b) in dag.succ(a).iter().enumerate() {
            assert!(a < b, "dag edge {a}->{b} not topological");
            got.push((a, b, labels.get(i).copied().unwrap_or(0)));
        }
    }
    assert_eq!(got, want);
    let rt: Vec<u32> = c.reverse_topo().collect();
    assert_eq!(rt.first().copied(), c.count().checked_sub(1));
}

#[test]
fn condensed_small() {
    let edges = [(0u32, 1u32, 1u8), (1, 0, 1), (1, 2, 2), (0, 2, 0), (2, 3, 2), (3, 2, 2), (3, 4, 1), (4, 4, 0)];
    let g = Graph::build_labelled(5, || edges.iter().copied());
    let c = Condensed::of(&g);
    check_condensed(&g, &c);
    assert_eq!(c.count(), 3);
    let (a, b, d) = (c.scc().comp(0), c.scc().comp(2), c.scc().comp(4));
    assert_eq!((a, b, d), (0, 1, 2));
    assert_eq!(c.dag().label(a, b), Some(0));
    assert_eq!(c.dag().label(b, d), Some(1));
    assert_eq!(c.dag().succ(a), &[b]);
    assert!(c.heap_bytes() > 0);
}

#[test]
fn condensed_random() {
    let mut rng = Rng(1234);
    for _ in 0..200 {
        let n = 1 + rng.below(80);
        let m = rng.below(3 * n + 1) as usize;
        let edges: Vec<(u32, u32, u8)> = (0..m).map(|_| (rng.below(n), rng.below(n), rng.below(4) as u8)).collect();
        let g = Graph::build_labelled(n, || edges.iter().copied());
        check_condensed(&g, &Condensed::of(&g));
        let plain = Graph::build(n, || edges.iter().map(|&(a, b, _)| (a, b)));
        check_condensed(&plain, &Condensed::of(&plain));
    }
}

/// Reference components by iterative Kosaraju (forward finish order, then reverse sweeps).
fn kosaraju(g: &Graph) -> Vec<u32> {
    let n = g.node_count() as usize;
    let mut seen = vec![false; n];
    let mut order = Vec::with_capacity(n);
    let mut stack: Vec<(u32, usize)> = Vec::new();
    for s in 0..n as u32 {
        if seen[s as usize] {
            continue;
        }
        seen[s as usize] = true;
        stack.push((s, 0));
        while let Some(&mut (v, ref mut i)) = stack.last_mut() {
            let succ = g.succ(v);
            if *i < succ.len() {
                let w = succ[*i];
                *i += 1;
                if !seen[w as usize] {
                    seen[w as usize] = true;
                    stack.push((w, 0));
                }
            } else {
                order.push(v);
                stack.pop();
            }
        }
    }
    let mut comp = vec![u32::MAX; n];
    let mut k = 0;
    let mut todo = Vec::new();
    for &s in order.iter().rev() {
        if comp[s as usize] != u32::MAX {
            continue;
        }
        comp[s as usize] = k;
        todo.push(s);
        while let Some(v) = todo.pop() {
            for &u in g.pred(v) {
                if comp[u as usize] == u32::MAX {
                    comp[u as usize] = k;
                    todo.push(u);
                }
            }
        }
        k += 1;
    }
    comp
}

/// Asserts that `a` and `b` describe the same partition of the nodes.
fn same_partition(a: &[u32], b: &[u32]) {
    assert_eq!(a.len(), b.len());
    let ka = a.iter().max().map_or(0, |&m| m as usize + 1);
    let kb = b.iter().max().map_or(0, |&m| m as usize + 1);
    assert_eq!(ka, kb, "component counts differ");
    let mut map = vec![u32::MAX; ka];
    for (v, (&x, &y)) in a.iter().zip(b).enumerate() {
        let slot = &mut map[x as usize];
        if *slot == u32::MAX {
            *slot = y;
        }
        assert_eq!(*slot, y, "node {v}: partitions differ");
    }
}

#[test]
fn scc_matches_kosaraju_medium() {
    let mut rng = Rng(99);
    for _ in 0..20 {
        let n = 1000 + rng.below(4000);
        let m = rng.below(3 * n) as usize;
        let edges = random_edges(&mut rng, n, m);
        let g = Graph::from_edges(n, &edges);
        let scc = Scc::of(&g);
        check_scc_structure(&g, &scc);
        same_partition(scc.comps(), &kosaraju(&g));
    }
}

const BIG: u32 = 1_000_000;

#[test]
fn large_chain_no_recursion() {
    // A million-deep DFS: recursion would overflow the stack.
    let g = Graph::build(BIG, || (0..BIG - 1).map(|v| (v, v + 1)));
    let c = Condensed::of(&g);
    assert_eq!(c.count(), BIG);
    assert!(c.scc().comps().iter().enumerate().all(|(v, &k)| k == v as u32));
    assert_eq!(c.dag().edge_count(), BIG - 1);
    assert_eq!(c.scc().nontrivial().count(), 0);
}

#[test]
fn large_reverse_chain() {
    let g = Graph::build(BIG, || (1..BIG).map(|v| (v, v - 1)));
    let scc = Scc::of(&g);
    assert_eq!(scc.count(), BIG);
    assert!(scc.comps().iter().enumerate().all(|(v, &k)| k == BIG - 1 - v as u32));
}

#[test]
fn large_single_cycle() {
    let g = Graph::build(BIG, || (0..BIG).map(|v| (v, (v + 1) % BIG)));
    let c = Condensed::of(&g);
    assert_eq!(c.count(), 1);
    assert_eq!(c.scc().size(0), BIG);
    assert!(c.scc().is_cyclic(&g, 0));
    assert_eq!(c.dag().edge_count(), 0);
}

#[test]
fn large_planted_blocks() {
    // Blocks of 5 nodes, each a cycle; block b links to b + 1 and to random later blocks, so
    // the components are exactly the blocks, in block order.
    const B: u32 = 5;
    let blocks = BIG / B;
    let mut rng = Rng(5);
    let mut edges = Vec::with_capacity(BIG as usize * 3);
    for b in 0..blocks {
        let base = b * B;
        for i in 0..B {
            edges.push((base + i, base + (i + 1) % B));
        }
        if b + 1 < blocks {
            edges.push((base + rng.below(B), base + B + rng.below(B)));
            let later = b + 1 + rng.below(blocks - b - 1);
            edges.push((base + rng.below(B), later * B + rng.below(B)));
        }
    }
    let g = Graph::from_edges(BIG, &edges);
    let c = Condensed::of(&g);
    assert_eq!(c.count(), blocks);
    for b in (0..blocks).step_by(9973) {
        let base = b * B;
        assert_eq!(c.scc().members(b), &[base, base + 1, base + 2, base + 3, base + 4]);
    }
    assert!(c.scc().comps().iter().enumerate().all(|(v, &k)| k == v as u32 / B));
    for a in c.topo() {
        assert!(c.dag().succ(a).iter().all(|&b| a < b));
    }
}

#[test]
fn large_random_matches_kosaraju() {
    let mut rng = Rng(0xdead_beef);
    let m = 3 * BIG as usize;
    let edges = random_edges(&mut rng, BIG, m);
    let g = Graph::from_edges(BIG, &edges);
    assert_eq!(g.node_count(), BIG);
    let c = Condensed::of(&g);
    check_scc_structure(&g, c.scc());
    same_partition(c.scc().comps(), &kosaraju(&g));
    // Average out-degree 3 on random digraphs: one giant component.
    let giant = (0..c.count()).map(|k| c.scc().size(k)).max().unwrap_or(0);
    assert!(giant > BIG / 2, "giant component {giant}");
    for a in c.topo() {
        assert!(c.dag().succ(a).iter().all(|&b| a < b));
    }
}
