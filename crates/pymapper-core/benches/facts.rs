//! FileFacts extraction benchmark: streaming read -> extract -> drop source, per-thread
//! fact stores and interners (the shape of the real loader), then reports time, throughput,
//! retained fact bytes per LOC (per table) and peak RSS.
//!
//! ```text
//! cargo bench -p pymapper-core --bench facts -- [--spans | --parse-only] [--reps N] [DIR ...]
//! ```
//!
//! `--spans` uses the pre-interning span sink instead of per-thread interners (then the
//! source would have to be kept; here it is still dropped, so this only measures speed).
//! `--parse-only` runs the parser with a no-op visitor instead (the baseline for the extractor's
//! own cost). Without DIRs it uses tests/corpus.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use pymapper_core::arena::PerThread;
use pymapper_core::extract::facts::{FactStore, FileFacts, SpanStrs, extract};
use pymapper_core::intern::Interner;
use pymapper_core::walk::{WalkOptions, walk};
use rayon::prelude::*;

#[derive(Default)]
struct Worker {
    store: FactStore,
    interner: Interner,
    spans: SpanStrs,
    files: Vec<FileFacts>,
    buf: Vec<u8>,
}

fn peak_rss() -> u64 {
    // SAFETY: `rusage` is plain old data; zeroed is a valid value and getrusage fills it in.
    let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
    // SAFETY: `ru` is a valid, writable `rusage` for the duration of the call.
    let rc = unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut ru) };
    if rc != 0 {
        return 0;
    }
    let v = ru.ru_maxrss as u64;
    if cfg!(target_os = "macos") { v } else { v * 1024 }
}

struct Rep {
    wall: f64,
    extract_ns: u64,
    bytes: u64,
    workers: Vec<Worker>,
}

fn run(paths: &pymapper_core::walk::PathList, spans: bool, parse_only: bool) -> Rep {
    let workers = PerThread::new(Worker::default);
    let extract_ns = AtomicU64::new(0);
    let bytes = AtomicU64::new(0);
    let t0 = Instant::now();
    (0..paths.len()).into_par_iter().for_each(|i| {
        workers.with(|_, w| {
            w.buf.clear();
            let Ok(mut f) = std::fs::File::open(paths.get(i)) else { return };
            if f.read_to_end(&mut w.buf).is_err() {
                return;
            }
            let t = Instant::now();
            if parse_only {
                pymapper_core::extract::parse(&w.buf, pymapper_core::extract::NoopVisitor);
                extract_ns.fetch_add(t.elapsed().as_nanos() as u64, Ordering::Relaxed);
                bytes.fetch_add(w.buf.len() as u64, Ordering::Relaxed);
                return;
            }
            let ff = if spans {
                extract(&w.buf, &mut w.store, &mut w.spans)
            } else {
                extract(&w.buf, &mut w.store, &mut w.interner)
            };
            extract_ns.fetch_add(t.elapsed().as_nanos() as u64, Ordering::Relaxed);
            bytes.fetch_add(w.buf.len() as u64, Ordering::Relaxed);
            w.files.push(ff);
        })
    });
    let wall = t0.elapsed().as_secs_f64();
    Rep { wall, extract_ns: extract_ns.into_inner(), bytes: bytes.into_inner(), workers: workers.into_vec() }
}

fn main() {
    let mut args = std::env::args().skip(1).filter(|a| a != "--bench");
    let mut reps = 3usize;
    let mut spans = false;
    let mut parse_only = false;
    let mut dirs = Vec::new();
    while let Some(a) = args.next() {
        match a.as_str() {
            "--reps" => reps = args.next().and_then(|v| v.parse().ok()).unwrap_or(reps),
            "--spans" => spans = true,
            "--parse-only" => parse_only = true,
            _ => dirs.push(PathBuf::from(a)),
        }
    }
    if dirs.is_empty() {
        dirs.push(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/corpus"));
    }
    let w = walk(&dirs, &WalkOptions::everything());
    println!("files: {}, threads: {}, sink: {}", w.files.len(), rayon::current_num_threads(), if spans { "spans" } else { "interner" });

    // Best time of `reps` runs. Only the last run's stores stay alive (the previous ones are
    // dropped before the next run starts), so peak RSS is that of one run retaining all facts.
    let (mut wall, mut extract_ns, mut bytes) = (f64::INFINITY, u64::MAX, 0);
    let mut last: Option<Vec<Worker>> = None;
    for _ in 0..reps.max(1) {
        drop(last.take());
        let r = run(&w.files, spans, parse_only);
        wall = wall.min(r.wall);
        extract_ns = extract_ns.min(r.extract_ns);
        bytes = r.bytes;
        last = Some(r.workers);
    }
    let rss_run = peak_rss();
    let Some(mut workers) = last else { return };
    let mb = bytes as f64 / 1e6;
    let cpu = extract_ns as f64 / 1e9;
    println!(
        "read+extract wall {:.1} ms, {:.1} MB, {:.0} MB/s all cores, {:.0} MB/s/core (extract only, {:.1} ms cpu)",
        wall * 1e3,
        mb,
        mb / wall,
        mb / cpu,
        cpu * 1e3
    );

    // Retained facts.
    let mut loc = 0u64;
    let mut rows = [0u64; 13];
    let mut row_bytes = 0usize;
    let mut heap = 0usize;
    let mut intern_heap = 0usize;
    let mut headers = 0usize;
    for wk in &workers {
        let st = &wk.store;
        for ff in &wk.files {
            loc += ff.lines as u64;
        }
        headers += wk.files.len() * std::mem::size_of::<FileFacts>();
        row_bytes += st.row_bytes();
        heap += st.heap_bytes();
        intern_heap += wk.interner.heap_bytes();
        let t = [
            (st.defs.len() as usize, 0),
            (st.decorators.len() as usize, 1),
            (st.bases.len() as usize, 2),
            (st.params.len() as usize, 3),
            (st.imports.len() as usize, 4),
            (st.calls.len() as usize, 5),
            (st.bindings.len() as usize, 6),
            (st.decls.len() as usize, 7),
            (st.names.len(), 8),
            (st.exports.len(), 9),
            (st.evidence.len(), 10),
            (st.diags.len() as usize, 11),
            (st.suppressions.len() as usize, 12),
        ];
        for (n, i) in t {
            rows[i] += n as u64;
        }
    }
    use pymapper_core::extract::facts::*;
    let row_size = [
        Defs::ROW_BYTES,
        Decorators::ROW_BYTES,
        Bases::ROW_BYTES,
        Params::ROW_BYTES,
        Imports::ROW_BYTES,
        Calls::ROW_BYTES,
        Bindings::ROW_BYTES,
        Decls::ROW_BYTES,
        4,
        4,
        4,
        Diags::ROW_BYTES,
        Suppressions::ROW_BYTES,
    ];
    let names = [
        "defs", "decorators", "bases", "params", "imports", "calls", "bindings", "decls", "names", "exports",
        "evidence", "diags", "suppressions",
    ];
    let loc_f = loc.max(1) as f64;
    println!("LOC: {loc}");
    println!("{:<13} {:>10} {:>10} {:>8}", "table", "rows", "rows/kLOC", "B/LOC");
    for i in 0..13 {
        println!(
            "{:<13} {:>10} {:>10.1} {:>8.2}",
            names[i],
            rows[i],
            rows[i] as f64 * 1000.0 / loc_f,
            (rows[i] as usize * row_size[i]) as f64 / loc_f
        );
    }
    println!(
        "facts rows {:.2} B/LOC (+ headers {:.2} B/LOC); table heap {:.2} B/LOC incl. spare capacity; interner heap {:.1} MB",
        row_bytes as f64 / loc_f,
        headers as f64 / loc_f,
        heap as f64 / loc_f,
        intern_heap as f64 / 1e6
    );
    let mut shrunk = 0usize;
    for wk in &mut workers {
        wk.store.shrink_to_fit();
        shrunk += wk.store.heap_bytes();
    }
    println!("table heap after shrink_to_fit {:.2} B/LOC", shrunk as f64 / loc_f);
    println!("peak RSS {:.1} MB (one run retaining all facts and interners)", rss_run as f64 / 1e6);
}
