//! Output allocates per run, not per finding: counts heap allocations (with a counting global
//! allocator) while sorting, baselining and writing reports of 1k and 50k findings.

use std::alloc::{GlobalAlloc, Layout, System};
use std::fmt;
use std::io;
use std::sync::atomic::{AtomicU64, Ordering};

use pymapper_core::config::OutputFormat;
use pymapper_core::output::{Baseline, Context, Options, Outcome, Report, Summary};
use pymapper_core::rules::codes::{COMPLEX_FUNCTION, UNUSED_IMPORT};
use pymapper_core::rules::{Arg, ArgResolver, FileId, Findings, Location, SymbolId};

struct Counting;

static ALLOCS: AtomicU64 = AtomicU64::new(0);

// SAFETY: forwards every call unchanged to the system allocator, which upholds the
// `GlobalAlloc` contract; the counter has no effect on the returned memory.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCS.fetch_add(1, Ordering::Relaxed);
        // SAFETY: same contract as our caller's (non-zero-size layout).
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: `ptr` was allocated by `System` with `layout` (every allocation goes there).
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        ALLOCS.fetch_add(1, Ordering::Relaxed);
        // SAFETY: `ptr` was allocated by `System` with `layout`; the caller guarantees
        // `new_size` is valid for it.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

/// Synthetic names: file `i` is `pkg/mod_{i}.py`, symbol `i` is `pkg.mod_{i % 1000}:f{i}`.
struct Ctx;

fn write_num(out: &mut Vec<u8>, v: u32) {
    let mut digits = [0u8; 10];
    let mut i = digits.len();
    let mut v = v;
    loop {
        i -= 1;
        digits[i] = b'0' + (v % 10) as u8;
        v /= 10;
        if v == 0 {
            break;
        }
    }
    out.extend_from_slice(&digits[i..]);
}

impl Context for Ctx {
    fn write_path(&self, file: FileId, out: &mut Vec<u8>) {
        out.extend_from_slice(b"pkg/mod_");
        write_num(out, file.0);
        out.extend_from_slice(b".py");
    }
    fn write_symbol_id(&self, sym: SymbolId, out: &mut Vec<u8>) {
        out.extend_from_slice(b"pkg.mod_");
        write_num(out, sym.0 % 1000);
        out.extend_from_slice(b":f");
        write_num(out, sym.0);
    }
}

impl ArgResolver for Ctx {
    fn write_symbol(&self, id: SymbolId, out: &mut dyn fmt::Write) -> fmt::Result {
        write!(out, "pkg.mod_{}:f{}", id.0 % 1000, id.0)
    }
    fn write_name(&self, id: u32, out: &mut dyn fmt::Write) -> fmt::Result {
        write!(out, "name{id}")
    }
    fn write_file(&self, id: FileId, out: &mut dyn fmt::Write) -> fmt::Result {
        write!(out, "pkg/mod_{}.py", id.0)
    }
    fn write_contract(&self, _: u32, out: &mut dyn fmt::Write) -> fmt::Result {
        out.write_str("c")
    }
}

fn findings(n: u32) -> Findings {
    let mut f = Findings::with_capacity(n as usize, 3 * n as usize);
    for i in 0..n {
        let loc = Location::new(FileId(i % 1000), i / 1000 + 1, 5);
        if i % 2 == 0 {
            f.push(COMPLEX_FUNCTION, loc, SymbolId(i), 100, &[Arg::Symbol(SymbolId(i)), Arg::U32(20), Arg::U32(10)]);
        } else {
            f.push(UNUSED_IMPORT, loc, SymbolId(i), 90, &[Arg::Name(i)]);
        }
    }
    f
}

fn count<T>(run: impl FnOnce() -> T) -> (T, u64) {
    let before = ALLOCS.load(Ordering::Relaxed);
    let v = run();
    (v, ALLOCS.load(Ordering::Relaxed) - before)
}

#[test]
fn allocations_do_not_grow_with_findings() {
    let mut per_n = Vec::new();
    for n in [1_000u32, 50_000] {
        let f = findings(n);
        // Report: order indices + one path pool for 1000 files (a few doublings).
        let (report, new_allocs) = count(|| Report::new(&f, &Ctx));
        assert!(new_allocs <= 40, "Report::new: {new_allocs} allocations for {n} findings");
        let mut writes = Vec::new();
        for format in [OutputFormat::Concise, OutputFormat::Json, OutputFormat::Sarif, OutputFormat::Github] {
            let opts = Options::new(format);
            let (res, a) = count(|| report.write(&opts, &Summary::default(), &mut io::sink()));
            assert_eq!(res.expect("write"), Outcome::Written);
            assert!(a <= 12, "{format:?}: {a} allocations for {n} findings");
            writes.push(a);
        }
        let (res, a) = count(|| report.write_baseline(&mut io::sink()));
        assert_eq!(res.expect("write"), Outcome::Written);
        // The baseline map grows by doubling: logarithmic in the number of keys.
        assert!(a <= 120, "baseline: {a} allocations for {n} findings");
        let baseline = Baseline::from_report(&report);
        drop(report);
        let mut report = Report::new(&f, &Ctx);
        let (dropped, a) = count(|| report.apply_baseline(&baseline));
        assert_eq!(dropped, n);
        assert!(a <= 16, "apply_baseline: {a} allocations for {n} findings");
        per_n.push(writes);
    }
    // Writing allocates the same at 1k and 50k findings (buffers are reused, never per line).
    assert_eq!(per_n[0], per_n[1], "allocations per format at 1k vs 50k findings");
}
