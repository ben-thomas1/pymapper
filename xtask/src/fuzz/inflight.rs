//! Per-worker "which iteration is running" words, optionally backed by a shared file mapping.
//!
//! Workers publish the iteration they are executing here (one store per input, no syscall). The
//! watchdog reads the words to detect hangs. When backed by a `MAP_SHARED` file, the words also
//! survive a process abort (stack overflow, OOM): the supervising parent reads the file after
//! the child dies and regenerates the in-flight inputs from `(seed, iteration)`.
//!
//! Layout (native-endian u64 words, one 128-byte line per entry to avoid false sharing):
//! line 0 is the header `[MAGIC, seed, slots, next_iter]`, line `1 + i` holds slot `i`'s
//! current iteration in word 0.

use std::fs::{self, File, OpenOptions};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, bail};

/// Slot word value: no input running.
pub const IDLE: u64 = u64::MAX;
/// Slot word value: the watchdog declared this slot's input hung and abandoned the worker.
pub const HUNG: u64 = u64::MAX - 1;
/// Iterations must stay below this value.
pub const MAX_ITER: u64 = HUNG;

const MAGIC: u64 = 0x5059_4d46_555a_5a31; // "PYMFUZZ1"
const LINE_WORDS: usize = 16;
const LINE_BYTES: usize = LINE_WORDS * 8;
const H_MAGIC: usize = 0;
const H_SEED: usize = 1;
const H_SLOTS: usize = 2;
const H_NEXT: usize = 3;

#[repr(C, align(128))]
struct Line([AtomicU64; LINE_WORDS]);

const _: () = assert!(size_of::<Line>() == LINE_BYTES);

enum Backing {
    Heap(Box<[Line]>),
    Mapped { ptr: *mut Line, lines: usize },
}

/// The slot words plus the shared next-iteration counter.
pub struct Inflight {
    backing: Backing,
    slots: usize,
}

// SAFETY: the mapping is only ever accessed through `AtomicU64`s, which are Sync; the raw
// pointer is owned by this value and unmapped exactly once in Drop.
unsafe impl Send for Inflight {}
// SAFETY: as above; all shared access goes through atomics.
unsafe impl Sync for Inflight {}

impl Inflight {
    /// Heap-backed (in-process only).
    pub fn heap(slots: usize, seed: u64, start_iter: u64) -> Inflight {
        let lines: Box<[Line]> = (0..slots + 1).map(|_| Line(std::array::from_fn(|_| AtomicU64::new(0)))).collect();
        let me = Inflight { backing: Backing::Heap(lines), slots };
        me.init(seed, start_iter);
        me
    }

    /// Backed by a new `MAP_SHARED` mapping of `path` (created or truncated).
    pub fn mapped(path: &Path, slots: usize, seed: u64, start_iter: u64) -> anyhow::Result<Inflight> {
        let lines = slots + 1;
        let bytes = lines * LINE_BYTES;
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)
            .with_context(|| format!("creating {}", path.display()))?;
        file.set_len(bytes as u64).with_context(|| format!("sizing {}", path.display()))?;
        let ptr = map(&file, bytes).with_context(|| format!("mapping {}", path.display()))?;
        let me = Inflight { backing: Backing::Mapped { ptr, lines }, slots };
        me.init(seed, start_iter);
        Ok(me)
    }

    fn lines(&self) -> &[Line] {
        match &self.backing {
            Backing::Heap(b) => b,
            // SAFETY: `ptr` is a live, page-aligned (so 128-byte aligned) mapping of exactly
            // `lines * LINE_BYTES` read/write bytes owned by `self`; zero bytes are valid
            // AtomicU64s and all access goes through atomics.
            Backing::Mapped { ptr, lines } => unsafe { std::slice::from_raw_parts(*ptr, *lines) },
        }
    }

    fn init(&self, seed: u64, start_iter: u64) {
        let lines = self.lines();
        for line in &lines[1..] {
            line.0[0].store(IDLE, Ordering::Relaxed);
        }
        let h = &lines[0].0;
        h[H_SEED].store(seed, Ordering::Relaxed);
        h[H_SLOTS].store(self.slots as u64, Ordering::Relaxed);
        h[H_NEXT].store(start_iter, Ordering::Relaxed);
        h[H_MAGIC].store(MAGIC, Ordering::Release);
    }

    pub fn slots(&self) -> usize {
        self.slots
    }

    /// Slot `i`'s current-iteration word.
    #[inline]
    pub fn slot(&self, i: usize) -> &AtomicU64 {
        &self.lines()[1 + i].0[0]
    }

    /// The shared counter workers claim iterations from.
    #[inline]
    pub fn next_iter(&self) -> &AtomicU64 {
        &self.lines()[0].0[H_NEXT]
    }
}

impl Drop for Inflight {
    fn drop(&mut self) {
        if let Backing::Mapped { ptr, lines } = self.backing {
            // SAFETY: `ptr`/`lines` describe the mapping created in `mapped`, unmapped only here.
            unsafe { libc::munmap(ptr.cast(), lines * LINE_BYTES) };
        }
    }
}

fn map(file: &File, bytes: usize) -> std::io::Result<*mut Line> {
    use std::os::fd::AsRawFd;
    // SAFETY: plain FFI call with a valid fd and a length equal to the file size; the result is
    // checked against MAP_FAILED before use.
    let p = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            bytes,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED,
            file.as_raw_fd(),
            0,
        )
    };
    if p == libc::MAP_FAILED {
        return Err(std::io::Error::last_os_error());
    }
    Ok(p.cast())
}

/// What a dead child left behind in its in-flight file.
#[derive(Debug, PartialEq, Eq)]
pub struct Snapshot {
    pub seed: u64,
    pub next_iter: u64,
    /// Iterations that were executing (or declared hung) when the child died, ascending.
    pub running: Vec<u64>,
}

/// Reads an in-flight file written through [`Inflight::mapped`].
pub fn read_snapshot(path: &Path) -> anyhow::Result<Snapshot> {
    let bytes = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let word = |i: usize| -> u64 {
        let mut w = [0u8; 8];
        w.copy_from_slice(&bytes[i * 8..i * 8 + 8]);
        u64::from_ne_bytes(w)
    };
    if bytes.len() < LINE_BYTES || bytes.len() % LINE_BYTES != 0 || word(H_MAGIC) != MAGIC {
        bail!("{} is not an in-flight file", path.display());
    }
    let slots = word(H_SLOTS) as usize;
    if bytes.len() / LINE_BYTES != slots + 1 {
        bail!("{}: slot count does not match the file size", path.display());
    }
    let mut running = Vec::new();
    for i in 0..slots {
        let it = word((1 + i) * LINE_WORDS);
        if it < MAX_ITER {
            running.push(it);
        }
    }
    running.sort_unstable();
    Ok(Snapshot { seed: word(H_SEED), next_iter: word(H_NEXT), running })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn heap_slots_start_idle() {
        let f = Inflight::heap(3, 9, 100);
        assert_eq!(f.slots(), 3);
        for i in 0..3 {
            assert_eq!(f.slot(i).load(Ordering::Relaxed), IDLE);
        }
        assert_eq!(f.next_iter().fetch_add(5, Ordering::Relaxed), 100);
        assert_eq!(f.next_iter().load(Ordering::Relaxed), 105);
    }

    #[test]
    fn mapped_file_survives_the_writer() {
        let path = std::env::temp_dir().join(format!("xtask-inflight-{}", std::process::id()));
        {
            let f = Inflight::mapped(&path, 4, 77, 10).unwrap();
            f.slot(1).store(42, Ordering::Release);
            f.slot(3).store(17, Ordering::Release);
            f.slot(2).store(HUNG, Ordering::Release);
            f.next_iter().store(64, Ordering::Release);
            // Dropped without any explicit flush: MAP_SHARED writes land in the page cache.
        }
        let snap = read_snapshot(&path).unwrap();
        assert_eq!(snap, Snapshot { seed: 77, next_iter: 64, running: vec![17, 42] });
        fs::write(&path, b"short").unwrap();
        assert!(read_snapshot(&path).is_err());
        fs::remove_file(&path).unwrap();
    }
}
