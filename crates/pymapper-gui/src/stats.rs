//! Frame-time samples and process resource usage.

use std::time::Duration;

/// Fixed-capacity ring of duration samples (nanoseconds). Allocates once up front; pushing
/// never allocates. When full, the oldest sample is overwritten.
#[derive(Debug, Clone)]
pub struct Samples {
    buf: Vec<u64>,
    head: usize,
}

/// Order statistics over a set of samples (nearest-rank percentiles).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Summary {
    pub count: usize,
    pub mean: Duration,
    pub p50: Duration,
    pub p95: Duration,
    pub p99: Duration,
    pub max: Duration,
}

impl Samples {
    /// `cap` must be non-zero.
    pub fn with_capacity(cap: usize) -> Self {
        assert!(cap > 0, "Samples capacity must be non-zero");
        Self { buf: Vec::with_capacity(cap), head: 0 }
    }

    pub fn push(&mut self, d: Duration) {
        let ns = u64::try_from(d.as_nanos()).unwrap_or(u64::MAX);
        if self.buf.len() < self.buf.capacity() {
            self.buf.push(ns);
        } else {
            self.buf[self.head] = ns;
            self.head += 1;
            if self.head == self.buf.len() {
                self.head = 0;
            }
        }
    }

    pub fn len(&self) -> usize {
        self.buf.len()
    }

    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    pub fn clear(&mut self) {
        self.buf.clear();
        self.head = 0;
    }

    /// Summarises the retained samples. `scratch` is reused for sorting.
    pub fn summary(&self, scratch: &mut Vec<u64>) -> Summary {
        let n = self.buf.len();
        if n == 0 {
            return Summary::default();
        }
        scratch.clear();
        scratch.extend_from_slice(&self.buf);
        scratch.sort_unstable();
        let total: u128 = scratch.iter().map(|&v| u128::from(v)).sum();
        let mean = u64::try_from(total / n as u128).unwrap_or(u64::MAX);
        let rank = |p: usize| -> Duration {
            // Nearest rank: ceil(p/100 * n), 1-based.
            let r = (p * n).div_ceil(100).max(1);
            Duration::from_nanos(scratch[r - 1])
        };
        Summary {
            count: n,
            mean: Duration::from_nanos(mean),
            p50: rank(50),
            p95: rank(95),
            p99: rank(99),
            max: Duration::from_nanos(scratch[n - 1]),
        }
    }
}

/// CPU time and peak resident set size of this process so far.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProcessUsage {
    /// User + system CPU time, all threads.
    pub cpu: Duration,
    pub peak_rss_bytes: u64,
}

#[cfg(unix)]
pub fn process_usage() -> ProcessUsage {
    // SAFETY: `rusage` is a plain C struct of integers; all-zero is a valid value.
    let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
    // SAFETY: `ru` is a valid, writable `rusage`; RUSAGE_SELF is always a valid `who`.
    let rc = unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut ru) };
    if rc != 0 {
        return ProcessUsage { cpu: Duration::ZERO, peak_rss_bytes: 0 };
    }
    let tv = |t: libc::timeval| {
        Duration::from_secs(u64::try_from(t.tv_sec).unwrap_or(0))
            + Duration::from_micros(u64::try_from(t.tv_usec).unwrap_or(0))
    };
    let maxrss = u64::try_from(ru.ru_maxrss).unwrap_or(0);
    // ru_maxrss is bytes on macOS and kilobytes on Linux.
    let peak_rss_bytes = if cfg!(target_os = "macos") { maxrss } else { maxrss.saturating_mul(1024) };
    ProcessUsage { cpu: tv(ru.ru_utime) + tv(ru.ru_stime), peak_rss_bytes }
}

#[cfg(not(unix))]
pub fn process_usage() -> ProcessUsage {
    ProcessUsage { cpu: Duration::ZERO, peak_rss_bytes: 0 }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(v: u64) -> Duration {
        Duration::from_millis(v)
    }

    #[test]
    fn empty_summary_is_zero() {
        let s = Samples::with_capacity(4);
        assert!(s.is_empty());
        assert_eq!(s.summary(&mut Vec::new()), Summary::default());
    }

    #[test]
    fn nearest_rank_percentiles() {
        let mut s = Samples::with_capacity(100);
        // Push 100..=1 ms in reverse to check sorting.
        for v in (1..=100).rev() {
            s.push(ms(v));
        }
        let sum = s.summary(&mut Vec::new());
        assert_eq!(sum.count, 100);
        assert_eq!(sum.p50, ms(50));
        assert_eq!(sum.p95, ms(95));
        assert_eq!(sum.p99, ms(99));
        assert_eq!(sum.max, ms(100));
        assert_eq!(sum.mean, Duration::from_micros(50_500));
    }

    #[test]
    fn single_sample() {
        let mut s = Samples::with_capacity(8);
        s.push(ms(7));
        let sum = s.summary(&mut Vec::new());
        assert_eq!((sum.p50, sum.p99, sum.max, sum.mean), (ms(7), ms(7), ms(7), ms(7)));
    }

    #[test]
    fn ring_keeps_latest_and_never_grows() {
        let mut s = Samples::with_capacity(3);
        for v in 1..=10 {
            s.push(ms(v));
        }
        assert_eq!(s.len(), 3);
        assert_eq!(s.buf.capacity(), 3);
        let sum = s.summary(&mut Vec::new());
        assert_eq!((sum.p50, sum.max), (ms(9), ms(10)));
        s.clear();
        assert!(s.is_empty());
        s.push(ms(1));
        assert_eq!(s.summary(&mut Vec::new()).max, ms(1));
    }

    #[test]
    fn huge_durations_saturate() {
        let mut s = Samples::with_capacity(2);
        s.push(Duration::MAX);
        assert_eq!(s.summary(&mut Vec::new()).max, Duration::from_nanos(u64::MAX));
    }

    #[cfg(unix)]
    #[test]
    fn process_usage_is_monotonic() {
        let a = process_usage();
        // Burn a little CPU.
        let mut x = 0u64;
        for i in 0..5_000_000u64 {
            x = x.wrapping_mul(31).wrapping_add(i);
        }
        std::hint::black_box(x);
        let b = process_usage();
        assert!(b.cpu >= a.cpu);
        assert!(a.peak_rss_bytes > 0 && b.peak_rss_bytes >= a.peak_rss_bytes);
    }
}
