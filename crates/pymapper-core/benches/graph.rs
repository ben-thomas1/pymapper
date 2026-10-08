//! Graph benchmark on generated graphs: CSR build, iterative Tarjan SCC, condensation.
//!
//! ```text
//! cargo bench -p pymapper-core --bench graph -- <shape> [--nodes N] [--degree D] [--reps R]
//! ```
//!
//! Shapes (default 1M nodes, average out-degree 3):
//!
//! - `random`: uniform random edges (one giant SCC plus many singletons).
//! - `calls`: call-graph-like: 80% of edges stay within a window of 64 nearby nodes, the rest go
//!   to a skewed set of popular targets (most nodes small fan-out, a few hubs); mostly acyclic
//!   with small cycles.
//! - `chain`: a path `0 -> 1 -> ... -> n-1` (million-deep DFS, `n` components).
//! - `cycle`: one cycle through every node (one component).
//! - `blocks`: cycles of 5 nodes chained forward plus random forward edges (`n / 5` components).
//!
//! Times are best of R; the edge list is generated once (not timed). Peak RSS is process-wide,
//! so run one shape per process; `rss_before` is the peak after generating the edge list.

use std::time::{Duration, Instant};

use pymapper_core::graph::{Condensed, Graph, Scc};

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

fn peak_rss_mb() -> f64 {
    // SAFETY: `getrusage` only writes the struct we pass; zeroed is a valid `rusage`.
    let ru = unsafe {
        let mut ru: libc::rusage = std::mem::zeroed();
        libc::getrusage(libc::RUSAGE_SELF, &mut ru);
        ru
    };
    // macOS reports bytes, Linux kibibytes. MB = 10^6 bytes.
    let max = ru.ru_maxrss as f64;
    if cfg!(target_os = "macos") { max / 1e6 } else { max * 1024.0 / 1e6 }
}

fn edges(shape: &str, n: u32, degree: u32) -> Vec<(u32, u32)> {
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    let m = n as usize * degree as usize;
    let mut out = Vec::with_capacity(m + n as usize);
    match shape {
        "random" => {
            for _ in 0..m {
                out.push((rng.below(n), rng.below(n)));
            }
        }
        "calls" => {
            for _ in 0..m {
                let s = rng.below(n);
                let d = if rng.below(5) != 0 {
                    // Nearby (same module): mostly forward, sometimes back.
                    let off = rng.below(64);
                    if rng.below(8) == 0 { s.saturating_sub(off) } else { (s + off).min(n - 1) }
                } else {
                    // Popular targets: squaring a uniform value skews towards low ids.
                    let u = rng.below(1 << 16) as u64;
                    ((u * u * u64::from(n)) >> 32) as u32
                };
                out.push((s, d));
            }
        }
        "chain" => out.extend((0..n.saturating_sub(1)).map(|v| (v, v + 1))),
        "cycle" => out.extend((0..n).map(|v| (v, (v + 1) % n))),
        "blocks" => {
            let blocks = n / 5;
            for b in 0..blocks {
                let base = b * 5;
                out.extend((0..5).map(|i| (base + i, base + (i + 1) % 5)));
                if b + 1 < blocks {
                    for _ in 0..degree.saturating_sub(1) {
                        let later = b + 1 + rng.below(blocks - b - 1);
                        out.push((base + rng.below(5), later * 5 + rng.below(5)));
                    }
                }
            }
        }
        _ => panic!("unknown shape {shape} (random|calls|chain|cycle|blocks)"),
    }
    out
}

fn best<R>(reps: usize, mut f: impl FnMut() -> R) -> (Duration, R) {
    let mut best = Duration::MAX;
    let mut last = None;
    for _ in 0..reps {
        let t = Instant::now();
        let r = std::hint::black_box(f());
        best = best.min(t.elapsed());
        last = Some(r);
    }
    (best, last.expect("reps >= 1"))
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1e3
}

fn main() {
    let mut args = std::env::args().skip(1).filter(|a| a != "--bench");
    let mut shape = String::from("random");
    let (mut n, mut degree, mut reps) = (1_000_000u32, 3u32, 3usize);
    while let Some(a) = args.next() {
        let mut val = || args.next().and_then(|v| v.parse::<u64>().ok()).expect("numeric value");
        match a.as_str() {
            "--nodes" => n = val() as u32,
            "--degree" => degree = val() as u32,
            "--reps" => reps = (val() as usize).max(1),
            s => shape = s.to_owned(),
        }
    }
    let list = edges(&shape, n, degree);
    let rss_before = peak_rss_mb();
    let (t_build, g) = best(reps, || Graph::from_edges(n, &list));
    let (t_scc, scc) = best(reps, || Scc::of(&g));
    let (t_cond, c) = best(reps, || Condensed::of(&g));
    let nontrivial = scc.nontrivial().count();
    let largest = (0..scc.count()).map(|k| scc.size(k)).max().unwrap_or(0);
    let mb = |b: usize| b as f64 / 1e6;
    println!("shape={shape} nodes={n} input_edges={} distinct_edges={}", list.len(), g.edge_count());
    println!(
        "build {:.1} ms ({:.1} M edges/s) | scc {:.1} ms ({:.1} M nodes+edges/s) | condense (scc + dag) {:.1} ms",
        ms(t_build),
        list.len() as f64 / t_build.as_secs_f64() / 1e6,
        ms(t_scc),
        (f64::from(n) + f64::from(g.edge_count())) / t_scc.as_secs_f64() / 1e6,
        ms(t_cond),
    );
    println!(
        "sccs={} nontrivial={nontrivial} largest={largest} dag_edges={}",
        scc.count(),
        c.dag().edge_count()
    );
    println!(
        "graph {:.1} MB ({:.2} B/edge) | scc {:.1} MB | condensed {:.1} MB | edge list {:.1} MB | rss_before {:.1} MB | peak_rss {:.1} MB",
        mb(g.heap_bytes()),
        g.heap_bytes() as f64 / f64::from(g.edge_count().max(1)),
        mb(scc.heap_bytes()),
        mb(c.heap_bytes()),
        mb(list.capacity() * 8),
        rss_before,
        peak_rss_mb(),
    );
}
