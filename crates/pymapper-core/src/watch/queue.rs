//! The event queue between a backend thread and the consumer: raw paths appended without
//! deduplication (cheap on the backend's thread), handed over as a sorted, deduplicated
//! [`Batch`] once the debounce window closes.
//!
//! Buffers are swapped, not copied: the pending store and the consumer's batch trade their
//! allocations on every hand-over, so a steady stream of batches allocates nothing.

use std::path::Path;
use std::sync::{Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use super::WatchOptions;

/// Event flag: the path is (or was) a directory.
pub const FLAG_DIR: u8 = 1;

/// Paths stored in one byte buffer with `(start, end, flags)` entries.
#[derive(Debug, Default)]
pub(super) struct Store {
    buf: Vec<u8>,
    spans: Vec<(u32, u32, u8)>,
}

impl Store {
    fn clear(&mut self) {
        self.buf.clear();
        self.spans.clear();
    }

    fn len(&self) -> usize {
        self.spans.len()
    }

    #[inline]
    fn get(&self, i: usize) -> &[u8] {
        let (s, e, _) = self.spans[i];
        &self.buf[s as usize..e as usize]
    }

    /// Appends `parts` concatenated. False (nothing appended) past 4 GiB of bytes.
    fn push(&mut self, parts: &[&[u8]], flags: u8) -> bool {
        let n: usize = parts.iter().map(|p| p.len()).sum();
        let start = self.buf.len();
        let Ok(end) = u32::try_from(start + n) else { return false };
        for p in parts {
            self.buf.extend_from_slice(p);
        }
        self.spans.push((start as u32, end, flags));
        true
    }

    /// Sorts the entries by path and merges duplicates (flags OR-ed). The bytes of dropped
    /// duplicates stay in the buffer until the next clear.
    fn sort_dedup(&mut self) {
        let buf = &self.buf;
        let key = |&(s, e, _): &(u32, u32, u8)| &buf[s as usize..e as usize];
        self.spans.sort_unstable_by(|a, b| key(a).cmp(key(b)));
        let mut w = 0usize;
        for r in 0..self.spans.len() {
            if w > 0 && key(&self.spans[w - 1]) == key(&self.spans[r]) {
                self.spans[w - 1].2 |= self.spans[r].2;
            } else {
                self.spans[w] = self.spans[r];
                w += 1;
            }
        }
        self.spans.truncate(w);
    }
}

/// One debounced set of changes: every path reported since the previous batch, sorted and
/// deduplicated, plus whether the backend lost track of events (`rescan`).
#[derive(Debug, Default)]
pub struct Batch {
    store: Store,
    rescan: bool,
    events: u64,
    first: Option<Instant>,
    last: Option<Instant>,
}

impl Batch {
    pub fn new() -> Batch {
        Batch::default()
    }

    /// Distinct paths reported.
    pub fn len(&self) -> usize {
        self.store.len()
    }

    pub fn is_empty(&self) -> bool {
        self.store.len() == 0 && !self.rescan
    }

    /// Path `i` (sorted order) as bytes.
    #[inline]
    pub fn path_bytes(&self, i: usize) -> &[u8] {
        self.store.get(i)
    }

    /// Path `i` (sorted order).
    #[inline]
    pub fn path(&self, i: usize) -> &Path {
        use std::os::unix::ffi::OsStrExt;
        Path::new(std::ffi::OsStr::from_bytes(self.store.get(i)))
    }

    /// Flags of path `i` ([`FLAG_DIR`]).
    #[inline]
    pub fn flags(&self, i: usize) -> u8 {
        self.store.spans[i].2
    }

    /// Paths in sorted order.
    pub fn paths(&self) -> impl ExactSizeIterator<Item = &Path> + '_ {
        (0..self.len()).map(|i| self.path(i))
    }

    /// The backend dropped events or the watched tree changed under it (FSEvents
    /// `MustScanSubDirs`, inotify queue overflow, too many pending paths): the paths are
    /// incomplete and everything under the roots may have changed.
    pub fn rescan(&self) -> bool {
        self.rescan
    }

    /// Raw events received for this batch (before deduplication).
    pub fn events(&self) -> u64 {
        self.events
    }

    /// When the first event of the batch arrived (edit-to-update latency starts here).
    pub fn first_event(&self) -> Option<Instant> {
        self.first
    }

    /// When the last event of the batch arrived.
    pub fn last_event(&self) -> Option<Instant> {
        self.last
    }

    fn clear(&mut self) {
        self.store.clear();
        self.rescan = false;
        self.events = 0;
        self.first = None;
        self.last = None;
    }
}

/// Outcome of [`super::Watcher::wait`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Wait {
    /// A batch was written to the caller's [`Batch`].
    Ready,
    /// The timeout passed without a complete batch.
    Timeout,
    /// The watcher was stopped ([`super::Waker::stop`], or the backend failed).
    Stopped,
}

/// When a pending batch is complete: `quiet` after its last event, but no later than
/// `max_delay` after its first.
#[inline]
pub(super) fn ready_at(first: Instant, last: Instant, opts: &WatchOptions) -> Instant {
    (last + opts.quiet).min(first + opts.max_delay)
}

#[derive(Debug, Default)]
struct Pending {
    batch: Batch,
    stop: bool,
}

/// Pending events shared by the backend (producer) and the consumer.
#[derive(Debug)]
pub(super) struct Queue {
    state: Mutex<Pending>,
    wake: Condvar,
    opts: WatchOptions,
}

impl Queue {
    pub fn new(opts: WatchOptions) -> Queue {
        Queue { state: Mutex::new(Pending::default()), wake: Condvar::new(), opts }
    }

    fn lock(&self) -> MutexGuard<'_, Pending> {
        // Plain data, valid after any panic: recover from poisoning.
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Records events: each item of `paths` is a list of parts forming one path (the parts
    /// are concatenated, so backends can join a directory and a name without a copy).
    pub fn push<'a>(&self, paths: impl IntoIterator<Item = (&'a [&'a [u8]], u8)>) {
        let now = Instant::now();
        let mut st = self.lock();
        let was_empty = st.batch.events == 0 && !st.batch.rescan;
        let max = self.opts.max_pending;
        let b = &mut st.batch;
        for (parts, flags) in paths {
            b.events += 1;
            if b.rescan {
                continue;
            }
            if b.store.len() >= max || !b.store.push(parts, flags) {
                // Bounded memory under event storms: give up on the paths, rescan instead.
                b.store.clear();
                b.rescan = true;
            }
        }
        b.first.get_or_insert(now);
        b.last = Some(now);
        drop(st);
        // The consumer re-arms its timer on every event only when it is waiting for the
        // first one; later events just move the deadline it re-reads on wake-up.
        if was_empty {
            self.wake.notify_all();
        }
    }

    /// Records that events were lost: the next batch asks for a rescan.
    pub fn push_rescan(&self) {
        let now = Instant::now();
        let mut st = self.lock();
        let b = &mut st.batch;
        b.events += 1;
        b.store.clear();
        b.rescan = true;
        b.first.get_or_insert(now);
        b.last = Some(now);
        drop(st);
        self.wake.notify_all();
    }

    /// Stops the queue: waiters return [`Wait::Stopped`].
    pub fn stop(&self) {
        self.lock().stop = true;
        self.wake.notify_all();
    }

    pub fn stopped(&self) -> bool {
        self.lock().stop
    }

    /// Waits for a complete batch and swaps it into `out` (its old contents are dropped,
    /// its buffers reused). `timeout`: `None` waits until a batch or a stop.
    pub fn wait(&self, out: &mut Batch, timeout: Option<Duration>) -> Wait {
        let deadline = timeout.map(|t| Instant::now() + t);
        let mut st = self.lock();
        loop {
            if st.stop {
                return Wait::Stopped;
            }
            let now = Instant::now();
            let ready = match (st.batch.first, st.batch.last) {
                (Some(f), Some(l)) => Some(ready_at(f, l, &self.opts)),
                _ => None,
            };
            if let Some(r) = ready
                && r <= now
            {
                out.clear();
                std::mem::swap(out, &mut st.batch);
                drop(st);
                out.store.sort_dedup();
                return Wait::Ready;
            }
            if deadline.is_some_and(|d| d <= now) {
                return Wait::Timeout;
            }
            let until = match (ready, deadline) {
                (Some(r), Some(d)) => Some(r.min(d)),
                (r, d) => r.or(d),
            };
            st = match until {
                Some(u) => self.wake.wait_timeout(st, u - now).unwrap_or_else(PoisonError::into_inner).0,
                None => self.wake.wait(st).unwrap_or_else(PoisonError::into_inner),
            };
        }
    }
}
