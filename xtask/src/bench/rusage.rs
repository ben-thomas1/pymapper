//! Peak resident set size of the current process via `getrusage(2)`.

use std::io;
use std::mem::MaybeUninit;

/// Peak RSS of this process so far, in bytes.
pub fn peak_rss_bytes() -> io::Result<u64> {
    let mut ru = MaybeUninit::<libc::rusage>::zeroed();
    // SAFETY: `ru` points to writable storage of the right size and alignment for a `rusage`,
    // and RUSAGE_SELF is a valid `who` argument.
    let rc = unsafe { libc::getrusage(libc::RUSAGE_SELF, ru.as_mut_ptr()) };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `rusage` is plain integer data, so the zeroed bytes were already a valid value,
    // and getrusage succeeded and filled it in.
    let ru = unsafe { ru.assume_init() };
    let max = u64::try_from(ru.ru_maxrss).unwrap_or(0);
    // Darwin reports bytes; Linux and the BSDs report KiB.
    Ok(if cfg!(target_vendor = "apple") { max } else { max * 1024 })
}

/// `(instructions retired, CPU cycles)` of this process so far, all threads (macOS only:
/// `proc_pid_rusage` with `RUSAGE_INFO_V4`). Cycles count only time on a core, so deltas
/// compare code changes far more stably than wall time on a loaded machine.
#[cfg(target_vendor = "apple")]
pub fn cpu_counters() -> Option<(u64, u64)> {
    let mut ri = MaybeUninit::<libc::rusage_info_v4>::zeroed();
    // SAFETY: `ri` is writable storage of the size and alignment of a `rusage_info_v4`, which
    // is what the RUSAGE_INFO_V4 flavor fills in; `getpid()` is always a valid pid.
    let rc = unsafe { libc::proc_pid_rusage(libc::getpid(), libc::RUSAGE_INFO_V4, ri.as_mut_ptr().cast()) };
    if rc != 0 {
        return None;
    }
    // SAFETY: plain integer data, zero-initialised and filled in by the successful call.
    let ri = unsafe { ri.assume_init() };
    Some((ri.ri_instructions, ri.ri_cycles))
}

/// Not available on this platform.
#[cfg(not(target_vendor = "apple"))]
pub fn cpu_counters() -> Option<(u64, u64)> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpu_counters_grow() {
        let Some((i0, c0)) = cpu_counters() else { return };
        let mut x = 0u64;
        for k in 0..10_000_000u64 {
            x = std::hint::black_box(x.wrapping_mul(31).wrapping_add(k));
        }
        let (i1, c1) = cpu_counters().expect("worked once");
        assert!(i1 > i0 + 10_000_000 && c1 > c0, "{i0}->{i1} {c0}->{c1} ({x})");
    }

    #[test]
    fn peak_rss_is_plausible_and_grows() {
        let before = peak_rss_bytes().expect("getrusage");
        assert!(before > 256 * 1024, "peak RSS {before} is implausibly small");
        assert!(before < 64 << 30, "peak RSS {before} is implausibly large (unit bug?)");
        // Touch 64 MiB so it becomes resident.
        let mut big = vec![0u8; 64 << 20];
        for i in (0..big.len()).step_by(4096) {
            big[i] = 1;
        }
        std::hint::black_box(&big);
        let after = peak_rss_bytes().expect("getrusage");
        assert!(after >= before + (32 << 20), "before {before} after {after}");
    }
}
