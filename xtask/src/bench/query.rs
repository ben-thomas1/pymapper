//! The `query` benchmark target: latency of the core query API (`pymapper_core::query`) over
//! sampled targets, with one reused scratch (as the CLI and GUI use it).

use std::path::Path;
use std::time::Instant;

use pymapper_core::graph::module_nodes;
use pymapper_core::index::{Index, Inputs, NodeId, load};
use pymapper_core::query::{
    DepOptions, Direction, LookupOptions, PathOptions, Query, Scratch, SummaryOptions, WalkOptions,
};
use pymapper_core::resolve::{Attribution, resolve_calls, resolve_imports};
use pymapper_core::roots::Origin;
use pymapper_core::timing;

use super::targets::{Counters, ReadStats, corpus_options, walk_corpus};

/// Up to `k` evenly spaced items of `all`.
fn sample(all: &[NodeId], k: usize) -> Vec<NodeId> {
    if all.len() <= k {
        return all.to_vec();
    }
    (0..k).map(|i| all[i * all.len() / k]).collect()
}

/// Runs `f` on every target as phase `phase`; pushes `<kind>_queries`, `<kind>_avg_ns`,
/// `<kind>_max_us` and `<kind>_avg_items` (the mean of what `f` returns: a result size).
fn batch<T>(phase: &'static str, kind: &str, targets: &[T], counters: &mut Counters, mut f: impl FnMut(&T) -> usize) {
    let (mut total, mut worst, mut items) = (0u128, 0u128, 0u64);
    timing::time(phase, || {
        for t in targets {
            let t0 = Instant::now();
            let n = std::hint::black_box(f(t));
            let ns = t0.elapsed().as_nanos();
            total += ns;
            worst = worst.max(ns);
            items += n as u64;
        }
    });
    let n = targets.len().max(1) as u128;
    counters.push((format!("{kind}_queries"), targets.len() as u64));
    counters.push((format!("{kind}_avg_ns"), (total / n) as u64));
    counters.push((format!("{kind}_max_us"), (worst / 1000) as u64));
    counters.push((format!("{kind}_avg_items"), items / n as u64));
}

/// The last `k` segments of a qualified id (`:` counts as `.`), as a suffix query.
fn suffix(qid: &[u8], k: usize) -> Vec<u8> {
    let mut seen = 0;
    for (i, &c) in qid.iter().enumerate().rev() {
        if c == b'.' || c == b':' {
            seen += 1;
            if seen == k {
                return qid[i + 1..].to_vec();
            }
        }
    }
    qid.to_vec()
}

/// `query`: see [`super::targets::Target::Query`]. Lazy query tables are built in their own
/// phases (`query.names`, `query.imports`, `query.cycle_graph`) before the batches.
pub(super) fn query_target(root: &Path) -> anyhow::Result<(ReadStats, Counters)> {
    let (files, walk_errors, _) = walk_corpus(root, &corpus_options())?;
    let inputs = Inputs::from_root_dir(root, &files, Origin::FirstParty, b"corpus");
    drop(files);
    let ix = load(inputs);
    let imp = resolve_imports(&ix, &Attribution::new());
    let calls = resolve_calls(&ix, &imp);
    let q = timing::time("query.build", || Query::new(&ix, &imp, &calls));
    timing::time("query.names", || q.names().len());
    timing::time("query.imports", || q.import_graph().len());
    let mut c: Counters = Vec::new();
    let defs: Vec<NodeId> = (0..ix.node_count()).filter(|&v| ix.kind(v).is_def()).collect();
    let defs = sample(&defs, 1000);
    let mods = sample(&module_nodes(&ix), 200);
    // The cycle import graph and its SCCs are built on the first cycle query.
    timing::time("query.cycle_graph", || mods.first().map(|&m| q.import_cycles(m, &mut Scratch::default()).len()));
    run_batches(&ix, &q, &defs, &mods, &mut c);
    let s = ix.stats();
    let stats = ReadStats { files: s.files, bytes: s.bytes, errors: walk_errors + ix.read_errors().len() as u64 };
    let mut counters = vec![
        ("loc".to_owned(), s.lines),
        ("nodes".to_owned(), u64::from(ix.node_count())),
        ("call_edges".to_owned(), u64::from(q.call_graph().edge_count())),
        ("query_kb".to_owned(), q.heap_bytes() as u64 / 1000),
        ("names_kb".to_owned(), q.names().heap_bytes() as u64 / 1000),
        ("calls_kb".to_owned(), calls.heap_bytes() as u64 / 1000),
        ("index_kb".to_owned(), ix.heap_bytes() as u64 / 1000),
    ];
    counters.append(&mut c);
    Ok((stats, counters))
}

fn run_batches(ix: &Index, q: &Query<'_>, defs: &[NodeId], mods: &[NodeId], c: &mut Counters) {
    let mut s = Scratch::default();
    let lo = LookupOptions::default();
    let qids: Vec<Vec<u8>> = defs
        .iter()
        .map(|&v| {
            let mut b = Vec::new();
            ix.qid(v, &mut b);
            b
        })
        .collect();
    let names: Vec<Vec<u8>> = qids.iter().map(|b| suffix(b, 1)).collect();
    let pairs: Vec<Vec<u8>> = qids.iter().map(|b| suffix(b, 2)).collect();
    // Fuzzy: the name with its first byte dropped (an edit), 100 samples (O(names) each).
    let typos: Vec<Vec<u8>> = names.iter().take(100).filter(|n| n.len() > 3).map(|n| n[1..].to_vec()).collect();
    batch("query.lookup_exact", "lookup_exact", &qids, c, |b| q.lookup(b, &lo).len());
    batch("query.lookup_name", "lookup_name", &names, c, |b| q.lookup(b, &lo).len());
    batch("query.lookup_suffix2", "lookup_suffix2", &pairs, c, |b| q.lookup(b, &lo).len());
    batch("query.lookup_fuzzy", "lookup_fuzzy", &typos, c, |b| q.lookup(b, &lo).len());
    let d1 = WalkOptions::default();
    let all = WalkOptions::transitive();
    batch("query.callers1", "callers1", defs, c, |&v| q.callers(v, &d1, &mut s).hops.len());
    batch("query.callees3", "callees3", defs, c, |&v| q.callees(v, &WalkOptions { depth: 3, ..d1 }, &mut s).hops.len());
    batch("query.callers_all", "callers_all", defs, c, |&v| q.callers(v, &all, &mut s).hops.len());
    batch("query.impact", "impact", defs, c, |&v| q.impact(v, &all, &mut s).walk.hops.len());
    let po = PathOptions::default();
    // Reachable pairs: the deepest transitive caller of each def (with callers) to the def.
    let reachable: Vec<(NodeId, NodeId)> =
        defs.iter().filter_map(|&v| q.callers(v, &all, &mut s).hops.last().map(|h| (h.node, v))).collect();
    batch("query.path", "path", &reachable, c, |&(a, b)| q.path(a, b, &po, &mut s).map_or(0, |p| p.nodes.len()));
    // Mostly unreachable pairs: def `i` to def `n - 1 - i`.
    let ends: Vec<(NodeId, NodeId)> = defs.iter().zip(defs.iter().rev()).map(|(&a, &b)| (a, b)).collect();
    batch("query.path_random", "path_random", &ends, c, |&(a, b)| q.path(a, b, &po, &mut s).map_or(0, |p| p.nodes.len()));
    let dop = DepOptions::default();
    batch("query.deps_out", "deps_out", mods, c, |&v| q.deps(v, Direction::Out, &dop, &mut s).deps.len());
    batch("query.deps_in", "deps_in", mods, c, |&v| q.deps(v, Direction::In, &dop, &mut s).deps.len());
    batch("query.import_cycles", "import_cycles", mods, c, |&v| q.import_cycles(v, &mut s).len());
    batch("query.call_cycle", "call_cycle", defs, c, |&v| q.call_cycle(v, &d1, &mut s).map_or(0, |h| h.members.len()));
    batch("query.summary", "summary", &[()], c, |_| q.summary(&SummaryOptions::default()).cognitive.len());
    c.push(("scratch_kb".to_owned(), s.heap_bytes() as u64 / 1000));
}
