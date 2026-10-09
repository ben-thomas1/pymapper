//! `--stats`: phase timings and peak RSS on stderr.

use std::fmt::Write as _;
use std::io::{self, Write};
use std::mem::MaybeUninit;
use std::time::Duration;

use pymapper_core::timing;

/// Peak RSS of this process so far, in bytes (`None` if `getrusage` fails).
pub fn peak_rss_bytes() -> Option<u64> {
    let mut ru = MaybeUninit::<libc::rusage>::zeroed();
    // SAFETY: `ru` points to writable storage of the right size and alignment for a `rusage`,
    // and RUSAGE_SELF is a valid `who` argument.
    let rc = unsafe { libc::getrusage(libc::RUSAGE_SELF, ru.as_mut_ptr()) };
    if rc != 0 {
        return None;
    }
    // SAFETY: `rusage` is plain integer data, so the zeroed bytes were already a valid value,
    // and getrusage succeeded and filled it in.
    let ru = unsafe { ru.assume_init() };
    let max = u64::try_from(ru.ru_maxrss).unwrap_or(0);
    // Darwin reports bytes; Linux and the BSDs report KiB.
    Some(if cfg!(target_vendor = "apple") { max } else { max * 1024 })
}

/// Renders the recorded phases (wall time, in first-recorded order), `counters`, the total and
/// the peak RSS.
pub fn render(phases: &[timing::Phase], total: Duration, counters: &[(&str, u64)], peak_rss: Option<u64>) -> String {
    let mut text = String::with_capacity(48 * (phases.len() + counters.len() + 3));
    text.push_str("pymapper: stats\n");
    // Writing into a String cannot fail.
    for p in phases {
        let _ = write!(text, "  {:<20} {:>10.1} ms", p.name, p.elapsed.as_secs_f64() * 1e3);
        if p.count > 1 {
            let _ = write!(text, " (x{})", p.count);
        }
        text.push('\n');
    }
    let _ = writeln!(text, "  {:<20} {:>10.1} ms", "total", total.as_secs_f64() * 1e3);
    for (name, value) in counters {
        let _ = writeln!(text, "  {name:<20} {value:>10}");
    }
    match peak_rss {
        Some(b) => {
            let _ = writeln!(text, "  {:<20} {:>10.1} MB", "peak_rss", b as f64 / 1e6);
        }
        None => text.push_str("  peak_rss                unknown\n"),
    }
    text
}

/// Takes the recorded phases and writes [`render`] to stderr.
pub fn report(total: Duration, counters: &[(&str, u64)]) {
    let text = render(&timing::take(), total, counters, peak_rss_bytes());
    // Ignore write failures: stderr may be closed and there is nowhere else to report.
    let _ = io::stderr().lock().write_all(text.as_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_phases_counters_and_rss() {
        let phases = [
            timing::Phase { name: "roots", elapsed: Duration::from_micros(1500), count: 1 },
            timing::Phase { name: "load.extract", elapsed: Duration::from_millis(20), count: 2 },
        ];
        let text = render(&phases, Duration::from_millis(30), &[("files", 12)], Some(5_000_000));
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[0], "pymapper: stats");
        assert!(lines[1].starts_with("  roots") && lines[1].ends_with("1.5 ms"), "{}", lines[1]);
        assert!(lines[2].ends_with("20.0 ms (x2)"), "{}", lines[2]);
        assert!(lines[3].starts_with("  total") && lines[3].ends_with("30.0 ms"));
        assert!(lines[4].starts_with("  files") && lines[4].ends_with(" 12"));
        assert!(lines[5].starts_with("  peak_rss") && lines[5].ends_with("5.0 MB"));
        assert!(render(&[], Duration::ZERO, &[], None).contains("peak_rss                unknown"));
    }

    #[test]
    fn peak_rss_is_plausible() {
        let b = peak_rss_bytes().expect("getrusage");
        assert!(b > 256 * 1024 && b < 64 << 30, "{b}");
    }
}
