//! Tests of the reach module: bit sets, HLL accuracy, forward reachability and reach sizes
//! against brute force on generated graphs, and root sets of loaded projects.

use super::*;
use crate::graph::{Condensed, Graph};

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

/// `m` random edges over `n` nodes; with `forward_bias`, most edges go from lower to higher
/// ids (call-graph-like: mostly acyclic with some cycles).
fn random_graph(seed: u64, n: u32, m: usize, forward_bias: bool) -> Graph {
    let mut rng = Rng(seed);
    let mut edges = Vec::with_capacity(m);
    for _ in 0..m {
        let a = rng.below(n);
        let b = rng.below(n);
        let e = if forward_bias && rng.below(10) != 0 { (a.min(b), a.max(b)) } else { (a, b) };
        edges.push(e);
    }
    Graph::from_edges(n, &edges)
}

/// Random weights: about 3/4 of the nodes are symbols, lines 0..40.
fn random_weights(seed: u64, n: u32) -> Weights {
    let mut rng = Rng(seed ^ 0xabcd);
    let mut symbol = BitSet::new(n);
    let mut lines = Vec::with_capacity(n as usize);
    for v in 0..n {
        if rng.below(4) != 0 {
            symbol.insert(v);
        }
        lines.push(rng.below(40));
    }
    Weights { symbol, lines }
}

/// Reference: BFS from every node over the original graph.
fn brute_sizes(g: &Graph, w: &Weights) -> Vec<[u64; 2]> {
    let n = g.node_count();
    let mut out = Vec::with_capacity(n as usize);
    for s in 0..n {
        let r = forward(g, [s]);
        let mut sum = [0u64; 2];
        for v in r.live().ones() {
            sum[0] += u64::from(w.symbol.contains(v));
            sum[1] += u64::from(w.lines[v as usize]);
        }
        out.push(sum);
    }
    out
}

// -- bit set -----------------------------------------------------------------------------

#[test]
fn bitset_basics() {
    let mut b = BitSet::new(130);
    assert_eq!(b.len(), 130);
    assert_eq!(b.count(), 0);
    assert!(b.insert_new(0));
    assert!(b.insert_new(63));
    assert!(b.insert_new(64));
    assert!(b.insert_new(129));
    assert!(!b.insert_new(64));
    assert!(b.contains(63) && b.contains(129) && !b.contains(1));
    assert_eq!(b.ones().collect::<Vec<_>>(), vec![0, 63, 64, 129]);
    assert_eq!(b.count(), 4);
    b.remove(63);
    assert_eq!(b.ones().collect::<Vec<_>>(), vec![0, 64, 129]);
    b.clear();
    assert_eq!(b.ones().next(), None);
    assert_eq!(BitSet::new(0).ones().next(), None);
}

#[test]
#[should_panic(expected = "out of range")]
fn bitset_rejects_out_of_range() {
    BitSet::new(10).insert(10);
}

// -- HyperLogLog -------------------------------------------------------------------------

#[test]
fn hll_empty_and_small() {
    let h = hll::Hll::new(10);
    assert_eq!(h.estimate(), 0.0);
    let mut h = hll::Hll::new(10);
    h.insert(hll::key(1, 0));
    let e = h.estimate();
    assert!((0.9..1.1).contains(&e), "{e}");
}

#[test]
fn hll_duplicates_count_once_and_merge_is_union() {
    let mut a = hll::Hll::new(8);
    let mut b = hll::Hll::new(8);
    for i in 0..5000 {
        a.insert(hll::key(i, 7));
        a.insert(hll::key(i, 7));
    }
    for i in 2500..7500 {
        b.insert(hll::key(i, 7));
    }
    let mut u = a.clone();
    u.merge(&b);
    let mut direct = hll::Hll::new(8);
    for i in 0..7500 {
        direct.insert(hll::key(i, 7));
    }
    assert_eq!(u, direct, "merge equals the sketch of the union");
    let mut again = u.clone();
    again.merge(&a);
    assert_eq!(again, u, "merge is idempotent");
}

/// Mean relative error over many independent sets stays within the documented standard error
/// bound for every precision and across the small, mid and large ranges.
#[test]
fn hll_error_within_documented_bound() {
    for p in [6u8, 8, 10, 12] {
        let se = hll::rel_std_error(p);
        for n in [10u32, 100, 300, 1000, 5000, 20_000, 100_000] {
            let trials = 40u32;
            let mut sq = 0.0;
            let mut bias = 0.0;
            for t in 0..trials {
                let mut h = hll::Hll::new(p);
                for i in 0..n {
                    h.insert(hll::key(i, t * 1_000_003 + p as u32));
                }
                let rel = h.estimate() / f64::from(n) - 1.0;
                sq += rel * rel;
                bias += rel;
            }
            let rmse = (sq / f64::from(trials)).sqrt();
            let bias = bias / f64::from(trials);
            // RMSE is about `se` (smaller in the near-exact small range); allow sampling noise.
            assert!(rmse < 1.5 * se, "p={p} n={n}: rmse {rmse:.4} vs se {se:.4}");
            assert!(bias.abs() < 0.6 * se, "p={p} n={n}: bias {bias:.4} vs se {se:.4}");
        }
    }
}

#[test]
fn hll_saturated_registers_do_not_blow_up() {
    // Every register at the maximum rank: huge but finite.
    let p = 4;
    let regs = vec![33 - p; hll::registers(p)];
    let e = hll::estimate(&regs, p);
    assert!(e.is_finite() && e > 1e9, "{e}");
}

#[test]
#[should_panic(expected = "out of range")]
fn hll_rejects_bad_precision() {
    hll::Hll::new(3);
}

// -- forward reachability ----------------------------------------------------------------

#[test]
fn forward_reaches_closure_with_shortest_paths() {
    // 0 -> 1 -> 2 -> 3, 0 -> 3, 4 -> 0 (4 unreached), 5 isolated, cycle 2 <-> 6.
    let g = Graph::from_edges(7, &[(0, 1), (1, 2), (2, 3), (0, 3), (4, 0), (2, 6), (6, 2)]);
    let r = forward(&g, [0]);
    assert_eq!(r.live().ones().collect::<Vec<_>>(), vec![0, 1, 2, 3, 6]);
    assert_eq!(r.count(), 5);
    assert_eq!(r.path_to(3), vec![0, 3], "BFS: shortest path");
    assert_eq!(r.path_to(6), vec![0, 1, 2, 6]);
    assert_eq!(r.path_to(0), vec![0]);
    assert!(r.path_to(4).is_empty() && r.via(4).is_none() && r.via(0).is_none());
    // Duplicate and multiple roots.
    let r = forward(&g, [5, 4, 5]);
    assert_eq!(r.live().ones().collect::<Vec<_>>(), vec![0, 1, 2, 3, 4, 5, 6]);
    assert!(forward(&g, []).live().ones().next().is_none());
    assert_eq!(forward(&Graph::from_edges(0, &[]), []).count(), 0);
}

#[test]
fn forward_matches_reference_on_random_graphs() {
    for seed in 1..20u64 {
        let n = 1 + (seed as u32 * 37) % 300;
        let g = random_graph(seed, n, n as usize * 2, seed % 2 == 0);
        let roots: Vec<u32> = (0..n).filter(|v| v % 17 == (seed as u32 % 17)).collect();
        let r = forward(&g, roots.iter().copied());
        // Reference: fixpoint iteration.
        let mut live = vec![false; n as usize];
        for &x in &roots {
            live[x as usize] = true;
        }
        loop {
            let mut changed = false;
            for v in 0..n {
                if live[v as usize] {
                    for &w in g.succ(v) {
                        if !live[w as usize] {
                            live[w as usize] = true;
                            changed = true;
                        }
                    }
                }
            }
            if !changed {
                break;
            }
        }
        for v in 0..n {
            assert_eq!(r.contains(v), live[v as usize], "seed {seed} node {v}");
            if let Some(p) = r.via(v) {
                assert!(g.has_edge(p, v) && r.contains(p));
            }
            let path = r.path_to(v);
            if r.contains(v) {
                assert!(roots.contains(&path[0]) && *path.last().unwrap() == v);
                for e in path.windows(2) {
                    assert!(g.has_edge(e[0], e[1]));
                }
            }
        }
    }
}

#[test]
fn forward_handles_a_long_chain() {
    let n = 200_000u32;
    let g = Graph::build(n, || (0..n - 1).map(|v| (v, v + 1)));
    let r = forward(&g, [0]);
    assert_eq!(r.count(), n);
    assert_eq!(r.path_to(n - 1).len(), n as usize);
}

// -- reach sizes -------------------------------------------------------------------------

fn check_exact(g: &Graph, w: &Weights) {
    let cd = Condensed::of(g);
    let want = brute_sizes(g, w);
    let got = reach_sizes(&cd, w, SizeMode::Exact);
    for v in 0..g.node_count() {
        assert_eq!(u64::from(got.symbols(v)), want[v as usize][0], "symbols of {v}");
        assert_eq!(u64::from(got.lines(v)), want[v as usize][1], "lines of {v}");
    }
}

#[test]
fn exact_sizes_on_hand_graphs() {
    // Diamond with a shared sink: 0 -> {1, 2} -> 3; the sink counts once.
    let g = Graph::from_edges(4, &[(0, 1), (0, 2), (1, 3), (2, 3)]);
    let w = Weights::uniform(4, 10);
    let cd = Condensed::of(&g);
    let s = reach_sizes(&cd, &w, SizeMode::Exact);
    assert_eq!((s.symbols(0), s.lines(0)), (4, 40));
    assert_eq!((s.symbols(1), s.lines(1)), (2, 20));
    assert_eq!((s.symbols(3), s.lines(3)), (1, 10));
    // Cycle members share their sizes; self loops are harmless.
    let g = Graph::from_edges(5, &[(0, 1), (1, 2), (2, 0), (2, 3), (3, 3)]);
    check_exact(&g, &Weights::uniform(5, 1));
    let s = reach_sizes(&Condensed::of(&g), &Weights::uniform(5, 1), SizeMode::Exact);
    assert_eq!([s.symbols(0), s.symbols(1), s.symbols(2), s.symbols(3), s.symbols(4)], [4, 4, 4, 1, 1]);
    // Empty graph.
    let g = Graph::from_edges(0, &[]);
    for mode in [SizeMode::Exact, SizeMode::default()] {
        let s = reach_sizes(&Condensed::of(&g), &Weights::uniform(0, 1), mode);
        assert_eq!(s.heap_bytes(), 0);
    }
}

#[test]
fn exact_sizes_match_brute_force() {
    for seed in 1..25u64 {
        let n = 1 + (seed as u32 * 53) % 400;
        let g = random_graph(seed, n, (n as usize * 3) / 2, seed % 3 != 0);
        check_exact(&g, &random_weights(seed, n));
    }
}

/// Small sets stay sparse, so the approximate mode is exact on them.
#[test]
fn approx_is_exact_below_the_sparse_threshold() {
    for seed in 1..25u64 {
        let n = 1 + (seed as u32 * 29) % 150;
        let g = random_graph(seed, n, n as usize, true);
        let mut w = random_weights(seed, n);
        for l in &mut w.lines {
            *l %= 3;
        }
        let cd = Condensed::of(&g);
        let want = reach_sizes(&cd, &w, SizeMode::Exact);
        // p = 12: sets of up to 1,024 nodes and 4,096 lines are exact; these graphs have at most
        // 150 nodes and 300 lines.
        let got = reach_sizes(&cd, &w, SizeMode::Approx(12));
        for v in 0..n {
            assert_eq!((got.symbols(v), got.lines(v)), (want.symbols(v), want.lines(v)), "seed {seed} node {v}");
        }
        assert_eq!(got.stats().dense_sets, 0);
    }
}

/// Sets of at most `2^p / 4` nodes and `2^p` lines are exact; sizes above `2^p / 4` of
/// larger sets are estimates within the documented error: their RMSE is about
/// `1.04 / sqrt(2^p)`, and no estimate is off by more than 5 standard errors.
#[test]
fn approx_error_bounded_on_large_sets() {
    for (seed, p) in [(3u64, 8u8), (5, 10), (7, 10), (11, 12)] {
        let n = 3000;
        let g = random_graph(seed, n, 9000, true);
        let w = random_weights(seed, n);
        let cd = Condensed::of(&g);
        let want = reach_sizes(&cd, &w, SizeMode::Exact);
        let got = reach_sizes(&cd, &w, SizeMode::Approx(p));
        // Node counts of the reach sets: every node a symbol.
        let nodes = reach_sizes(&cd, &Weights::uniform(n, 0), SizeMode::Exact);
        let se = hll::rel_std_error(p);
        let m = hll::registers(p) as u32;
        let big = m / 4;
        let (mut sq, mut cnt) = (0.0, 0u32);
        for v in 0..n {
            let sparse = nodes.symbols(v) <= m / 4 && want.lines(v) <= m;
            for (a, b) in [(got.symbols(v), want.symbols(v)), (got.lines(v), want.lines(v))] {
                if sparse {
                    assert_eq!(a, b, "sparse sets are exact (p={p}, node {v})");
                    continue;
                }
                if b <= big {
                    continue;
                }
                let rel = f64::from(a) / f64::from(b) - 1.0;
                assert!(rel.abs() < 5.0 * se, "p={p} node {v}: {a} vs {b}");
                sq += rel * rel;
                cnt += 1;
            }
        }
        assert!(cnt > 100, "the graph must produce large sets");
        // Estimates of nested sets are correlated, so allow more than for independent sets.
        let rmse = (sq / f64::from(cnt)).sqrt();
        assert!(rmse < 2.0 * se, "p={p}: rmse {rmse:.4} vs se {se:.4}");
        assert!(got.stats().dense_sets > 0);
    }
}

#[test]
fn approx_is_deterministic_and_frees_sketches() {
    let n = 50_000u32;
    let g = random_graph(42, n, 120_000, true);
    let w = random_weights(42, n);
    let cd = Condensed::of(&g);
    let a = reach_sizes(&cd, &w, SizeMode::Approx(10));
    let b = reach_sizes(&cd, &w, SizeMode::Approx(10));
    assert_eq!(a, b);
    // A chain keeps at most one component's sketches alive at a time.
    let g = Graph::build(n, || (0..n - 1).map(|v| (v, v + 1)));
    let s = reach_sizes(&Condensed::of(&g), &Weights::uniform(n, 2), SizeMode::Approx(10));
    assert!(s.stats().peak_live <= 2, "{:?}", s.stats());
    assert!(s.stats().peak_dense <= 4, "{:?}", s.stats());
    assert_eq!(s.symbols(n - 1), 1);
    assert_eq!(s.symbols(n - 200), 200, "sparse: exact");
    let rel = f64::from(s.symbols(0)) / f64::from(n) - 1.0;
    assert!(rel.abs() < 5.0 * hll::rel_std_error(10), "{}", s.symbols(0));
    let rel = f64::from(s.lines(0)) / f64::from(2 * n) - 1.0;
    assert!(rel.abs() < 5.0 * hll::rel_std_error(10), "{}", s.lines(0));
}

#[test]
fn approx_handles_a_wide_fan_in_and_fan_out() {
    // A hub called by every node and calling a tail: the hub's dense sketch is shared by
    // thousands of predecessors (copied, not stolen, until the last one).
    let n = 5000u32;
    let hub = 0;
    let mut edges: Vec<(u32, u32)> = (1..n / 2).map(|v| (v, hub)).collect();
    edges.extend((n / 2..n).map(|v| (hub, v)));
    let g = Graph::from_edges(n, &edges);
    let w = Weights::uniform(n, 1);
    let cd = Condensed::of(&g);
    let want = reach_sizes(&cd, &w, SizeMode::Exact);
    let got = reach_sizes(&cd, &w, SizeMode::Approx(10));
    for v in 0..n {
        let rel = f64::from(got.symbols(v)) / f64::from(want.symbols(v)) - 1.0;
        assert!(rel.abs() < 5.0 * hll::rel_std_error(10), "node {v}");
    }
}

#[test]
#[should_panic(expected = "weights cover")]
fn sizes_reject_mismatched_weights() {
    let g = Graph::from_edges(3, &[(0, 1)]);
    reach_sizes(&Condensed::of(&g), &Weights::uniform(2, 1), SizeMode::Exact);
}
