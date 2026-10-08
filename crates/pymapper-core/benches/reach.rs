//! Reachability benchmark on generated graphs: forward BFS from roots, HLL reach sizes and
//! (optionally) exact reach sizes with their error.
//!
//! ```text
//! cargo bench -p pymapper-core --bench reach -- <shape> [--nodes N] [--degree D] [--reps R]
//!     [--precision P] [--exact]
//! ```
//!
//! Shapes as in the `graph` bench (default 1M nodes, average out-degree 3): `random`, `calls`,
//! `chain`, `blocks`. Every node is a symbol with 1..=30 own lines (mean 15.5). Roots for the
//! BFS: every 100th node. `--exact` also runs `SizeMode::Exact` and reports the relative error
//! of the estimated sizes above `2^p / 4` (sets of at most `2^p / 4` nodes and `2^p` lines
//! are exact).
//!
//! Times are best of R (graph generation and condensation are not timed). Peak RSS is
//! process-wide: run one configuration per process.

use std::time::{Duration, Instant};

use pymapper_core::graph::{Condensed, Graph};
use pymapper_core::reach::{BitSet, SizeMode, Weights, forward, hll, reach_sizes};

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
                    let off = rng.below(64);
                    if rng.below(8) == 0 { s.saturating_sub(off) } else { (s + off).min(n - 1) }
                } else {
                    let u = rng.below(1 << 16) as u64;
                    ((u * u * u64::from(n)) >> 32) as u32
                };
                out.push((s, d));
            }
        }
        "chain" => out.extend((0..n.saturating_sub(1)).map(|v| (v, v + 1))),
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
        _ => panic!("unknown shape {shape} (random|calls|chain|blocks)"),
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
    let mut shape = String::from("calls");
    let (mut n, mut degree, mut reps, mut p, mut exact) = (1_000_000u32, 3u32, 3usize, 10u8, false);
    while let Some(a) = args.next() {
        let mut val = || args.next().and_then(|v| v.parse::<u64>().ok()).expect("numeric value");
        match a.as_str() {
            "--nodes" => n = val() as u32,
            "--degree" => degree = val() as u32,
            "--reps" => reps = (val() as usize).max(1),
            "--precision" => p = val() as u8,
            "--exact" => exact = true,
            s => shape = s.to_owned(),
        }
    }
    let list = edges(&shape, n, degree);
    let g = Graph::from_edges(n, &list);
    drop(list);
    let cd = Condensed::of(&g);
    let mut rng = Rng(7);
    let mut symbol = BitSet::new(n);
    let mut lines = Vec::with_capacity(n as usize);
    for v in 0..n {
        symbol.insert(v);
        lines.push(1 + rng.below(30));
    }
    let w = Weights { symbol, lines };
    let total_lines: u64 = w.lines.iter().map(|&l| u64::from(l)).sum();
    let rss_before = peak_rss_mb();
    let (t_fwd, live) = best(reps, || forward(&g, (0..n).step_by(100)));
    let (t_approx, approx) = best(reps, || reach_sizes(&cd, &w, SizeMode::Approx(p)));
    let rss_approx = peak_rss_mb();
    println!(
        "shape={shape} nodes={n} edges={} sccs={} dag_edges={} total_lines={total_lines} precision={p} (se {:.2}%)",
        g.edge_count(),
        cd.count(),
        cd.dag().edge_count(),
        100.0 * hll::rel_std_error(p)
    );
    println!(
        "forward {:.1} ms ({} live, {:.1} M nodes+edges/s) | approx sizes {:.1} ms | max symbols {} lines {}",
        ms(t_fwd),
        live.count(),
        (f64::from(n) + f64::from(g.edge_count())) / t_fwd.as_secs_f64() / 1e6,
        ms(t_approx),
        (0..n).map(|v| approx.symbols(v)).max().unwrap_or(0),
        (0..n).map(|v| approx.lines(v)).max().unwrap_or(0),
    );
    let st = approx.stats();
    println!(
        "dense_sets={} peak_live={} peak_dense={} ({:.1} MB) peak_sparse={:.1} MB | rss_before {:.1} MB | rss_after_approx {:.1} MB",
        st.dense_sets,
        st.peak_live,
        st.peak_dense,
        st.peak_dense as f64 * 2.0 * (hll::registers(p) + 4 * hll::HIST) as f64 / 1e6,
        st.peak_sparse_bytes as f64 / 1e6,
        rss_before,
        rss_approx,
    );
    if exact {
        let (t_exact, ex) = best(1, || reach_sizes(&cd, &w, SizeMode::Exact));
        let big = (hll::registers(p) / 4) as u32;
        let (mut sq, mut cnt, mut worst) = (0.0f64, 0u64, 0.0f64);
        for v in 0..n {
            for (a, b) in [(approx.symbols(v), ex.symbols(v)), (approx.lines(v), ex.lines(v))] {
                if b > big {
                    let rel = f64::from(a) / f64::from(b) - 1.0;
                    sq += rel * rel;
                    cnt += 1;
                    worst = worst.max(rel.abs());
                }
            }
        }
        println!(
            "exact sizes {:.1} ms ({:.1}x approx) | estimated sets {cnt}: rmse {:.2}% worst {:.2}% | peak_rss {:.1} MB",
            ms(t_exact),
            t_exact.as_secs_f64() / t_approx.as_secs_f64(),
            100.0 * (sq / cnt.max(1) as f64).sqrt(),
            100.0 * worst,
            peak_rss_mb(),
        );
    }
}
