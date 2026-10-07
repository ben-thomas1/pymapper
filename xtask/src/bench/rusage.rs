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

#[cfg(test)]
mod tests {
    use super::*;

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
