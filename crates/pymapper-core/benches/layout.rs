//! Treemap layout benchmark on a real hierarchy (a venv, the stdlib, a project) or a
//! synthetic one of a given size.
//!
//! ```text
//! cargo bench -p pymapper-core --bench layout -- [--third DIR] [--stdlib DIR] [--first DIR]
//!     [--synthetic NODES] [--reps R] [--metric loc|symbols|cc|cog|uniform]
//! ```
//!
//! `--third DIR` (a `site-packages`): every top-level entry is one third-party distribution.
//! `--stdlib DIR`: one stdlib group. `--first DIR`: one first-party group. Without any DIR or
//! `--synthetic`, uses tests/corpus as first party.
//!
//! Loading (walk + extract + aggregates) is reported but not part of the layout numbers.
//! Layout numbers are best of R: tree build, each level, the full layout, cache save and load.
//! Peak RSS is process-wide (dominated by the index for real inputs); layout memory is reported
//! from its own byte counts.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use pymapper_core::index::{Inputs, load};
use pymapper_core::layout::{Layout, LayoutConfig, LayoutTree, Region, SizeMetric, TreeOptions, cache_path, load_levels, save_levels};
use pymapper_core::roots::{FileStatus, Origin};
use pymapper_core::rollup::Aggregates;
use pymapper_core::walk::{WalkOptions, walk};

fn peak_rss_mb() -> f64 {
    // SAFETY: `getrusage` only writes the struct we pass; zeroed is a valid `rusage`.
    let ru = unsafe {
        let mut ru: libc::rusage = std::mem::zeroed();
        libc::getrusage(libc::RUSAGE_SELF, &mut ru);
        ru
    };
    let max = ru.ru_maxrss as f64;
    if cfg!(target_os = "macos") { max / 1e6 } else { max * 1024.0 / 1e6 }
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1e3
}

/// Dotted module name of a path relative to its root; `None` if not importable.
fn module_of(rel: &[u8]) -> Option<Vec<u8>> {
    let stem = rel.strip_suffix(b".pyi").or_else(|| rel.strip_suffix(b".py"))?;
    let stem = stem.strip_suffix(b"/__init__").unwrap_or(stem);
    if stem == b"__init__" {
        return None;
    }
    let mut out = Vec::with_capacity(stem.len());
    for (k, comp) in stem.split(|&b| b == b'/').enumerate() {
        if comp.is_empty() || comp.contains(&b'.') || comp.contains(&b'-') {
            return None;
        }
        if k > 0 {
            out.push(b'.');
        }
        out.extend_from_slice(comp);
    }
    Some(out)
}

/// Adds the files under `root`: one group for all of them, or (`split`) one per top-level
/// entry.
fn add_root(inp: &mut Inputs, root: &Path, origin: Origin, name: &[u8], split: bool) {
    let mut opts = WalkOptions::everything();
    opts.sort = true;
    let w = walk(&[root], &opts);
    let rb = root.as_os_str().as_encoded_bytes();
    let rb = rb.strip_suffix(b"/").unwrap_or(rb);
    let one = if split { None } else { Some(inp.add_group(name, origin, None)) };
    let mut groups: std::collections::HashMap<Vec<u8>, u32> = std::collections::HashMap::new();
    for i in 0..w.files.len() {
        let p = w.files.get_bytes(i);
        let Some(rel) = p.strip_prefix(rb).and_then(|r| r.strip_prefix(b"/")) else { continue };
        let Some(m) = module_of(rel) else { continue };
        let g = match one {
            Some(g) => g,
            None => {
                let top = m.split(|&b| b == b'.').next().unwrap_or(&m).to_vec();
                *groups.entry(top.clone()).or_insert_with(|| inp.add_group(&top, origin, None))
            }
        };
        if inp.push(p, g, &m, FileStatus::Active).is_err() {
            break;
        }
    }
}

/// A synthetic venv-like hierarchy of about `n` nodes: distributions -> packages -> modules
/// -> classes/functions -> methods, heavy-tailed LOC. Returns (depths, metric, regions).
fn synthetic(n: usize) -> (Vec<u16>, Vec<u32>, Vec<Region>) {
    let mut s = 0x9e37_79b9_7f4a_7c15u64;
    let mut rnd = move |m: u64| {
        s ^= s >> 12;
        s ^= s << 25;
        s ^= s >> 27;
        s.wrapping_mul(0x2545_f491_4f6c_dd1d) % m
    };
    let (mut depths, mut metric, mut regions) = (Vec::with_capacity(n), Vec::with_capacity(n), Vec::new());
    while depths.len() < n {
        depths.push(0);
        metric.push(0);
        regions.push(match regions.len() {
            0 => Region::Main,
            1 => Region::Stdlib,
            _ => Region::ThirdParty,
        });
        for _ in 0..1 + rnd(12) {
            depths.push(1);
            metric.push(0);
            for _ in 0..1 + rnd(30) {
                depths.push(2);
                let lines = 20 + rnd(60).pow(2) as u32;
                metric.push(lines);
                for _ in 0..rnd(25) {
                    let class = rnd(4) == 0;
                    depths.push(3);
                    metric.push(5 + rnd(40) as u32);
                    if class {
                        for _ in 0..1 + rnd(12) {
                            depths.push(4);
                            metric.push(3 + rnd(30) as u32);
                        }
                    }
                }
            }
        }
    }
    (depths, metric, regions)
}

fn best<T>(reps: usize, mut f: impl FnMut() -> T) -> (Duration, T) {
    let mut bestd = Duration::MAX;
    let mut out = None;
    for _ in 0..reps.max(1) {
        let t = Instant::now();
        let v = f();
        bestd = bestd.min(t.elapsed());
        out = Some(v);
    }
    (bestd, out.unwrap_or_else(|| unreachable!("reps >= 1")))
}

fn main() {
    let mut args = std::env::args().skip(1).filter(|a| a != "--bench");
    let (mut third, mut stdlib, mut first) = (Vec::<PathBuf>::new(), None, None);
    let (mut synth, mut reps, mut metric) = (None, 5usize, SizeMetric::Loc);
    while let Some(a) = args.next() {
        let mut val = || args.next().unwrap_or_else(|| panic!("{a} needs a value"));
        match a.as_str() {
            "--third" => third.push(val().into()),
            "--stdlib" => stdlib = Some(PathBuf::from(val())),
            "--first" => first = Some(PathBuf::from(val())),
            "--synthetic" => synth = Some(val().parse::<usize>().expect("node count")),
            "--reps" => reps = val().parse().expect("reps"),
            "--metric" => {
                metric = match val().as_str() {
                    "loc" => SizeMetric::Loc,
                    "symbols" => SizeMetric::Symbols,
                    "cc" => SizeMetric::CcSum,
                    "cog" => SizeMetric::CogSum,
                    "uniform" => SizeMetric::Uniform,
                    m => panic!("unknown metric {m}"),
                }
            }
            _ => panic!("unknown argument {a}"),
        }
    }
    let opts = TreeOptions { metric, min_weight: 1 };
    let (tb, tree) = if let Some(n) = synth {
        let (d, m, r) = synthetic(n);
        best(reps, || LayoutTree::from_preorder(&d, &m, &r, 1))
    } else {
        if third.is_empty() && stdlib.is_none() && first.is_none() {
            first = Some(PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/corpus")));
        }
        let t0 = Instant::now();
        let mut inp = Inputs::new();
        if let Some(f) = &first {
            add_root(&mut inp, f, Origin::FirstParty, b"project", false);
        }
        for t in &third {
            add_root(&mut inp, t, Origin::ThirdParty, b"", true);
        }
        if let Some(s) = &stdlib {
            add_root(&mut inp, s, Origin::Stdlib, b"stdlib", false);
        }
        let files = inp.len();
        let ix = load(inp);
        let agg = Aggregates::build(&ix);
        let total_loc: u64 = (0..ix.groups().len() as u32).map(|g| u64::from(agg.loc()[ix.group_node(g) as usize])).sum();
        println!(
            "load: {files} files, {} groups, {} nodes, {total_loc} LOC in {:.0} ms (not part of the layout)",
            ix.groups().len(),
            ix.node_count(),
            ms(t0.elapsed())
        );
        best(reps, || LayoutTree::from_index(&ix, &agg, opts))
    };
    let tree = Arc::new(tree);
    println!("tree: {} nodes, {} levels, built in {:.2} ms, {:.1} MB", tree.len(), tree.levels(), ms(tb), tree.heap_bytes() as f64 / 1e6);
    let cfg = LayoutConfig::default();
    // Per level, best of R.
    let mut per_level = vec![Duration::MAX; tree.levels() as usize];
    let mut full = Duration::MAX;
    let mut layout = Layout::new(Arc::clone(&tree), cfg);
    for _ in 0..reps.max(1) {
        let mut l = Layout::new(Arc::clone(&tree), cfg);
        let t_all = Instant::now();
        for slot in per_level.iter_mut() {
            let t = Instant::now();
            l.step();
            *slot = (*slot).min(t.elapsed());
        }
        full = full.min(t_all.elapsed());
        layout = l;
    }
    for (d, t) in per_level.iter().enumerate() {
        println!("  level {d}: {:>9} nodes  {:>8.2} ms", tree.level(d as u32).len(), ms(*t));
    }
    let mb = layout.heap_bytes() as f64 / 1e6;
    println!("layout: all levels {:.2} ms ({:.1} ns/node), rects {mb:.1} MB", ms(full), full.as_nanos() as f64 / tree.len().max(1) as f64);
    let dir = std::env::temp_dir().join(format!("pymapper-layout-bench-{}", std::process::id()));
    let path = cache_path(&dir);
    let (ts, ()) = best(reps, || save_levels(&path, &tree, &cfg, layout.levels()).expect("save"));
    let (tl, back) = best(reps, || load_levels(&path, &tree, &cfg).expect("load"));
    let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    assert_eq!(back.len(), layout.levels().len());
    println!("cache: {:.1} MB, save {:.2} ms, load {:.2} ms", size as f64 / 1e6, ms(ts), ms(tl));
    let _ = std::fs::remove_dir_all(&dir);
    println!("peak RSS: {:.1} MB", peak_rss_mb());
}
