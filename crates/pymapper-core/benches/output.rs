//! Output benchmark: sorting, every format, and the baseline round trip on synthetic findings.
//!
//! ```text
//! cargo bench -p pymapper-core --bench output -- [N_FINDINGS] [N_FILES]
//! ```
//!
//! Defaults: 1,000,000 findings over 10,000 files, pushed in a scrambled order (worst case for
//! locality). With `SORTED=1` they are pushed interleaved over files in id order instead,
//! closer to what per-file analyses produce. Output goes to an in-memory sink that only counts bytes, so the
//! numbers are the formatting cost alone. Peak RSS is for the whole process.

use std::fmt;
use std::io;
use std::time::Instant;

use pymapper_core::config::OutputFormat;
use pymapper_core::output::{Baseline, Context, Options, Report, Summary};
use pymapper_core::rules::codes::{COMPLEX_FUNCTION, UNUSED_FUNCTION, UNUSED_IMPORT};
use pymapper_core::rules::{Arg, ArgResolver, FileId, Findings, Location, SymbolId};

/// Pre-rendered names (as the index renders them from interned strings, without `fmt`): file
/// `i` is `src/pkg_{i / 100}/mod_{i}.py`; symbol `s` is `pkg_{f / 100}.mod_{f}:func_{s}` with
/// `f = s % files`; name `n` is `name_{n}`.
struct Ctx {
    paths: Vec<String>,
    symbols: Vec<String>,
    names: Vec<String>,
}

impl Ctx {
    fn new(n: u32, files: u32) -> Ctx {
        Ctx {
            paths: (0..files).map(|i| format!("src/pkg_{}/mod_{i}.py", i / 100)).collect(),
            symbols: (0..n).map(|s| format!("pkg_{}.mod_{}:func_{s}", (s % files) / 100, s % files)).collect(),
            names: (0..n).map(|i| format!("name_{i}")).collect(),
        }
    }
}

impl Context for Ctx {
    fn write_path(&self, file: FileId, out: &mut Vec<u8>) {
        out.extend_from_slice(self.paths[file.0 as usize].as_bytes());
    }
    fn write_symbol_id(&self, sym: SymbolId, out: &mut Vec<u8>) {
        out.extend_from_slice(self.symbols[sym.0 as usize].as_bytes());
    }
}

impl ArgResolver for Ctx {
    fn write_symbol(&self, id: SymbolId, out: &mut dyn fmt::Write) -> fmt::Result {
        out.write_str(&self.symbols[id.0 as usize])
    }
    fn write_name(&self, id: u32, out: &mut dyn fmt::Write) -> fmt::Result {
        out.write_str(&self.names[id as usize])
    }
    fn write_file(&self, id: FileId, out: &mut dyn fmt::Write) -> fmt::Result {
        out.write_str(&self.paths[id.0 as usize])
    }
    fn write_contract(&self, _: u32, out: &mut dyn fmt::Write) -> fmt::Result {
        out.write_str("layers")
    }
}

/// Counts bytes; never fails.
struct Count(u64);

impl io::Write for Count {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0 += buf.len() as u64;
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn findings(n: u32, files: u32) -> Findings {
    let mut f = Findings::with_capacity(n as usize, 2 * n as usize);
    // Multiplicative scramble: a permutation of 0..n when gcd(K, n) = 1 (K is prime).
    let k: u64 = if std::env::var_os("SORTED").is_some() { 1 } else { 2_654_435_761 };
    for i in 0..n {
        let s = ((u64::from(i) * k) % u64::from(n)) as u32;
        let file = s % files;
        let loc = Location::new(FileId(file), s / files + 1, 5);
        match s % 3 {
            0 => f.push(COMPLEX_FUNCTION, loc, SymbolId(s), 100, &[Arg::Symbol(SymbolId(s)), Arg::U32(17), Arg::U32(10)]),
            1 => f.push(UNUSED_FUNCTION, loc, SymbolId(s), 85, &[Arg::Symbol(SymbolId(s))]),
            _ => f.push(UNUSED_IMPORT, loc, SymbolId(file), 90, &[Arg::Name(s)]),
        }
    }
    f
}

fn peak_rss_mib() -> f64 {
    // SAFETY: `getrusage` only writes the struct we pass; zeroed is a valid `rusage`.
    let ru = unsafe {
        let mut ru: libc::rusage = std::mem::zeroed();
        libc::getrusage(libc::RUSAGE_SELF, &mut ru);
        ru
    };
    // macOS reports bytes, Linux kibibytes.
    let max = ru.ru_maxrss as f64;
    if cfg!(target_os = "macos") { max / (1024.0 * 1024.0) } else { max / 1024.0 }
}

fn main() {
    let args: Vec<u32> = std::env::args().skip(1).filter_map(|a| a.parse().ok()).collect();
    let n = args.first().copied().unwrap_or(1_000_000);
    let files = args.get(1).copied().unwrap_or(10_000).max(1);
    let ctx = Ctx::new(n, files);
    let f = findings(n, files);
    println!("{n} findings over {files} files; buffer {:.1} MiB", (f.len() * 36) as f64 / (1024.0 * 1024.0));

    let t = Instant::now();
    let report = Report::new(&f, &ctx);
    let sort = t.elapsed();
    println!("{:<18} {:>8.1} ms  {:>6.0} ns/finding", "sort", sort.as_secs_f64() * 1e3, sort.as_secs_f64() * 1e9 / f64::from(n));

    for format in [OutputFormat::Concise, OutputFormat::Json, OutputFormat::Sarif, OutputFormat::Github] {
        let mut sink = Count(0);
        let t = Instant::now();
        report.write(&Options::new(format), &Summary::default(), &mut sink).expect("write");
        let d = t.elapsed().as_secs_f64();
        println!(
            "{:<18} {:>8.1} ms  {:>6.0} ns/finding  {:>7.1} MiB  {:>6.0} MiB/s",
            format!("write {format:?}"),
            d * 1e3,
            d * 1e9 / f64::from(n),
            sink.0 as f64 / (1024.0 * 1024.0),
            sink.0 as f64 / (1024.0 * 1024.0) / d
        );
    }

    let mut text = Vec::new();
    let t = Instant::now();
    report.write_baseline(&mut text).expect("write");
    let d = t.elapsed().as_secs_f64();
    println!("{:<18} {:>8.1} ms  {:>6.0} ns/finding  {:>7.1} MiB", "baseline save", d * 1e3, d * 1e9 / f64::from(n), text.len() as f64 / (1024.0 * 1024.0));
    drop(report);

    let t = Instant::now();
    let baseline = Baseline::parse(&text).expect("parse");
    let d = t.elapsed().as_secs_f64();
    println!("{:<18} {:>8.1} ms  {:>6.0} ns/entry ({} keys)", "baseline load", d * 1e3, d * 1e9 / baseline.len() as f64, baseline.len());
    drop(text);

    let mut report = Report::new(&f, &ctx);
    let t = Instant::now();
    let dropped = report.apply_baseline(&baseline);
    let d = t.elapsed().as_secs_f64();
    assert_eq!(dropped, n);
    println!("{:<18} {:>8.1} ms  {:>6.0} ns/finding", "baseline apply", d * 1e3, d * 1e9 / f64::from(n));
    println!("peak RSS {:.0} MiB", peak_rss_mib());
}
