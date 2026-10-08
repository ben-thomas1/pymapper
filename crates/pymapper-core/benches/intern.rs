//! Interner benchmark on real Python identifier distributions.
//!
//! ```text
//! cargo bench -p pymapper-core --bench intern -- <sharded|local|single> [--reps N] [DIR...]
//! ```
//!
//! Without DIRs it uses tests/corpus. Each file's identifiers are found with a simple scanner
//! (skips comments, string literals, numbers and keywords). Two phases are timed, each on a fresh
//! interner per repetition:
//!
//! - `intern`: identifiers pre-scanned into spans; pure interning cost (worst-case contention).
//! - `scan+intern`: scan and intern each file in one pass (closer to the real extractor, which
//!   interleaves lexing with interning).
//!
//! Run one mode per process so peak RSS is attributable.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use pymapper_core::arena::PerThread;
use pymapper_core::intern::{Interner, Sym, sharded};
use rayon::prelude::*;

const KEYWORDS: &[&[u8]] = &[
    b"False", b"None", b"True", b"and", b"as", b"assert", b"async", b"await", b"break", b"class",
    b"continue", b"def", b"del", b"elif", b"else", b"except", b"finally", b"for", b"from",
    b"global", b"if", b"import", b"in", b"is", b"lambda", b"nonlocal", b"not", b"or", b"pass",
    b"raise", b"return", b"try", b"while", b"with", b"yield",
];

fn is_id_start(b: u8) -> bool {
    b == b'_' || b.is_ascii_alphabetic() || b >= 0x80
}

fn is_id_cont(b: u8) -> bool {
    is_id_start(b) || b.is_ascii_digit()
}

fn is_string_prefix(w: &[u8]) -> bool {
    w.len() <= 3 && w.iter().all(|c| b"rRbBfFuUtT".contains(c))
}

/// Skips a string literal starting at the quote `src[i]`; returns the index after it.
fn skip_string(src: &[u8], mut i: usize) -> usize {
    let q = src[i];
    let triple = src.len() >= i + 3 && src[i + 1] == q && src[i + 2] == q;
    i += if triple { 3 } else { 1 };
    while i < src.len() {
        let c = src[i];
        if c == b'\\' {
            i += 2;
            continue;
        }
        if c == q {
            if !triple {
                return i + 1;
            }
            if src.len() >= i + 3 && src[i + 1] == q && src[i + 2] == q {
                return i + 3;
            }
        } else if c == b'\n' && !triple {
            return i + 1;
        }
        i += 1;
    }
    src.len()
}

/// Calls `f` for each identifier (non-keyword) in `src`.
#[inline]
fn scan(src: &[u8], mut f: impl FnMut(&[u8])) {
    let mut i = 0;
    while i < src.len() {
        let c = src[i];
        if c == b'#' {
            while i < src.len() && src[i] != b'\n' {
                i += 1;
            }
        } else if c == b'"' || c == b'\'' {
            i = skip_string(src, i);
        } else if is_id_start(c) {
            let s = i;
            while i < src.len() && is_id_cont(src[i]) {
                i += 1;
            }
            let w = &src[s..i];
            if i < src.len() && (src[i] == b'"' || src[i] == b'\'') && is_string_prefix(w) {
                i = skip_string(src, i);
            } else if !KEYWORDS.contains(&w) {
                f(w);
            }
        } else if c.is_ascii_digit() {
            while i < src.len() && (is_id_cont(src[i]) || src[i] == b'.') {
                i += 1;
            }
        } else {
            i += 1;
        }
    }
}

struct File {
    src: Vec<u8>,
    /// Flat `(start, len)` identifier spans.
    spans: Vec<u32>,
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let Ok(ft) = e.file_type() else { continue };
        let p = e.path();
        if ft.is_dir() {
            walk(&p, out);
        } else if ft.is_file()
            && p.extension().is_some_and(|x| x == "py" || x == "pyi")
        {
            out.push(p);
        }
    }
}

fn load(dirs: &[PathBuf]) -> Vec<File> {
    let mut paths = Vec::new();
    for d in dirs {
        walk(d, &mut paths);
    }
    paths.sort();
    paths
        .par_iter()
        .filter_map(|p| std::fs::read(p).ok())
        .map(|src| {
            let mut spans = Vec::new();
            scan(&src, |w| {
                let start = w.as_ptr().addr() - src.as_ptr().addr();
                spans.push(start as u32);
                spans.push(w.len() as u32);
            });
            File { src, spans }
        })
        .collect()
}

fn idents(f: &File) -> impl Iterator<Item = &[u8]> {
    f.spans.as_chunks::<2>().0.iter().map(|&[s, n]| &f.src[s as usize..(s + n) as usize])
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

fn mib(b: usize) -> f64 {
    b as f64 / (1024.0 * 1024.0)
}

/// Result of one repetition: timings of named sub-phases, total, distinct count, heap bytes,
/// and a checksum over resolved lengths to keep the work observable.
struct Rep {
    parts: Vec<(&'static str, Duration)>,
    total: Duration,
    distinct: usize,
    heap: usize,
    check: u64,
}

fn run_single(files: &[File], inline: bool) -> Rep {
    let t0 = Instant::now();
    let mut li = Interner::new();
    let mut out: Vec<Vec<Sym>> = Vec::with_capacity(files.len());
    for f in files {
        let mut ids = Vec::with_capacity(f.spans.len() / 2);
        if inline {
            scan(&f.src, |w| ids.push(li.intern(w)));
        } else {
            for w in idents(f) {
                ids.push(li.intern(w));
            }
        }
        out.push(ids);
    }
    let total = t0.elapsed();
    let check = checksum(&out, |s| li.resolve(s).len());
    Rep { parts: vec![], total, distinct: li.len(), heap: li.heap_bytes(), check }
}

fn run_sharded(files: &[File], inline: bool) -> Rep {
    let t0 = Instant::now();
    let si = sharded::ShardedInterner::new();
    let out: Vec<Vec<Sym>> = files
        .par_iter()
        .map(|f| {
            let mut ids = Vec::with_capacity(f.spans.len() / 2);
            if inline {
                scan(&f.src, |w| ids.push(si.intern(w)));
            } else {
                for w in idents(f) {
                    ids.push(si.intern(w));
                }
            }
            ids
        })
        .collect();
    let fz = si.freeze();
    let total = t0.elapsed();
    let check = checksum(&out, |s| fz.resolve(s).len());
    Rep { parts: vec![], total, distinct: fz.len(), heap: fz.heap_bytes(), check }
}

fn run_local(files: &[File], inline: bool) -> Rep {
    let t0 = Instant::now();
    let pt: PerThread<Interner> = PerThread::new(Interner::new);
    let mut out: Vec<(usize, Vec<Sym>)> = files
        .par_iter()
        .map(|f| {
            pt.with(|slot, li| {
                let mut ids = Vec::with_capacity(f.spans.len() / 2);
                if inline {
                    scan(&f.src, |w| ids.push(li.intern(w)));
                } else {
                    for w in idents(f) {
                        ids.push(li.intern(w));
                    }
                }
                (slot, ids)
            })
        })
        .collect();
    let t1 = Instant::now();
    let parts = pt.into_vec();
    let mut local_heap = 0;
    for p in &parts {
        local_heap += p.heap_bytes();
    }
    let (g, remaps) = Interner::merge_all(parts);
    let t2 = Instant::now();
    out.par_iter_mut().for_each(|(slot, ids)| remaps[*slot].apply(ids));
    let t3 = Instant::now();
    let mut remap_heap = 0;
    for r in &remaps {
        remap_heap += r.heap_bytes();
    }
    drop(remaps);
    let total = t3 - t0;
    let ids: Vec<Vec<Sym>> = out.into_iter().map(|(_, v)| v).collect();
    let check = checksum(&ids, |s| g.resolve(s).len());
    // Peak memory of the approach: all worker tables + remaps alive at merge time.
    let heap = g.heap_bytes() + local_heap + remap_heap;
    Rep {
        parts: vec![("par-intern", t1 - t0), ("merge", t2 - t1), ("remap", t3 - t2)],
        total,
        distinct: g.len(),
        heap,
        check,
    }
}

fn checksum(out: &[Vec<Sym>], len: impl Fn(Sym) -> usize) -> u64 {
    let mut c = 0u64;
    for ids in out {
        for &s in ids {
            c = c.wrapping_add(len(s) as u64);
        }
    }
    c
}

fn main() {
    let mut args = std::env::args().skip(1).filter(|a| a != "--bench");
    let mode = args.next().unwrap_or_else(|| "sharded".to_owned());
    let mut reps = 5usize;
    let mut dirs = Vec::new();
    while let Some(a) = args.next() {
        if a == "--reps" {
            reps = args.next().and_then(|v| v.parse().ok()).unwrap_or(reps);
        } else {
            dirs.push(PathBuf::from(a));
        }
    }
    if dirs.is_empty() {
        dirs.push(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/corpus"));
    }
    let run: fn(&[File], bool) -> Rep = match mode.as_str() {
        "sharded" => run_sharded,
        "local" => run_local,
        "single" => run_single,
        other => {
            eprintln!("unknown mode {other}: expected sharded | local | single");
            std::process::exit(2);
        }
    };

    let t = Instant::now();
    let files = load(&dirs);
    let bytes: usize = files.iter().map(|f| f.src.len()).sum();
    let n_ids: usize = files.iter().map(|f| f.spans.len() / 2).sum();
    println!(
        "corpus: {} files, {:.1} MiB, {} identifiers, loaded in {:.2?}, threads {}",
        files.len(),
        mib(bytes),
        n_ids,
        t.elapsed(),
        rayon::current_num_threads()
    );
    let rss0 = peak_rss();

    for inline in [false, true] {
        let phase = if inline { "scan+intern" } else { "intern" };
        let mut totals = Vec::new();
        let mut last = None;
        for _ in 0..reps {
            let r = run(&files, inline);
            totals.push(r.total);
            last = Some(r);
        }
        totals.sort();
        let r = last.expect("reps >= 1");
        let med = totals[totals.len() / 2];
        let mut parts = String::new();
        for (name, d) in &r.parts {
            parts.push_str(&format!(" {name}={:.1}ms", d.as_secs_f64() * 1e3));
        }
        println!(
            "{mode:>8} {phase:>12}: min {:.1}ms median {:.1}ms ({:.1} M ids/s) distinct {} heap {:.1} MiB check {}{}",
            totals[0].as_secs_f64() * 1e3,
            med.as_secs_f64() * 1e3,
            n_ids as f64 / totals[0].as_secs_f64() / 1e6,
            r.distinct,
            mib(r.heap),
            r.check,
            if parts.is_empty() { String::new() } else { format!(" [last:{parts}]") },
        );
    }
    let rss1 = peak_rss();
    println!(
        "peak RSS: after load {:.1} MiB, after runs {:.1} MiB (delta {:.1} MiB)",
        mib(rss0 as usize),
        mib(rss1 as usize),
        mib(rss1.saturating_sub(rss0) as usize)
    );
}
