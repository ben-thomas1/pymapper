//! Own file watching (design: GUI, watch mode; dependency policy: file watching is written
//! here, on libc / CoreServices FFI): native change notifications, debounced into batches,
//! classified against the loaded index into the arguments of
//! [`crate::incremental::Program::update`].
//!
//! ```text
//!   backend thread                     consumer (CLI loop / GUI map worker)
//!   FSEvents (macOS, dispatch queue)
//!   inotify  (Linux, reader thread) ──> Queue ──> Watcher::wait ──> Batch (sorted, unique paths)
//!                                     append      debounce               │
//!                                                                         └─> changes::classify
//!                                                                             ──> Changes { changed, rewalk, reload }
//!                                                                             ──> Program::update(inputs, changed)
//! ```
//!
//! # Debounce
//!
//! A batch closes [`WatchOptions::quiet`] after its last event (editors save with several
//! writes and renames within a few milliseconds), but no later than
//! [`WatchOptions::max_delay`] after its first, so a continuous stream of writes (a build,
//! a `git checkout` of thousands of files) still produces updates. Paths are appended
//! without deduplication on the backend's thread and sorted / deduplicated once per batch
//! on the consumer's. Past [`WatchOptions::max_pending`] paths the batch degrades to a
//! rescan, which keeps memory bounded under event storms.
//!
//! # Backends
//!
//! - macOS: one FSEvents stream over all roots with file-level events
//!   (`kFSEventStreamCreateFlagFileEvents | NoDefer`, 5 ms latency), delivered on a
//!   private serial dispatch queue. Dropped events (`MustScanSubDirs`, user/kernel dropped)
//!   and a moved or deleted root become a rescan.
//! - Linux: inotify, one watch per directory (added recursively at start and for every
//!   directory created or moved in), read by one thread polling the descriptor and a stop
//!   pipe. Hidden directories and `__pycache__` are not watched. Queue overflow and running
//!   out of watches become a rescan.
//!
//! Neither backend follows symlinks (like the walker). Cost while idle: zero CPU (blocked
//! in the kernel / libdispatch); memory: the pending paths (bytes + 12 bytes each) and, on
//! Linux, one path per watched directory.

mod changes;
mod queue;

#[cfg(target_os = "macos")]
#[path = "watch/fsevents.rs"]
mod backend;
#[cfg(target_os = "linux")]
#[path = "watch/inotify.rs"]
mod backend;

#[cfg(test)]
mod tests;

pub use changes::{Changes, classify, inputs_of, update};
pub use queue::{Batch, FLAG_DIR, Wait};

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use queue::Queue;

/// Debounce and memory limits of a [`Watcher`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WatchOptions {
    /// A batch closes this long after its last event.
    pub quiet: Duration,
    /// ... and at the latest this long after its first.
    pub max_delay: Duration,
    /// Distinct pending paths before the batch degrades to a rescan.
    pub max_pending: usize,
}

impl Default for WatchOptions {
    fn default() -> WatchOptions {
        WatchOptions { quiet: Duration::from_millis(DEFAULT_QUIET_MS), max_delay: Duration::from_millis(300), max_pending: 1 << 20 }
    }
}

/// Default [`WatchOptions::quiet`] in milliseconds.
pub const DEFAULT_QUIET_MS: u64 = 30;

/// Why a watcher could not start.
#[derive(Debug, thiserror::Error)]
pub enum WatchError {
    #[error("cannot watch {path}: {source}", path = .0.display(), source = .1)]
    Root(PathBuf, #[source] std::io::Error),
    #[error("file watching failed: {0}")]
    Backend(&'static str),
    #[error("file watching failed: {0}")]
    Io(#[source] std::io::Error),
    #[error("file watching is not supported on this platform")]
    Unsupported,
}

/// Watches directory trees and hands out debounced [`Batch`]es (see the module docs).
/// Dropping it stops the backend.
pub struct Watcher {
    queue: Arc<Queue>,
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    backend: backend::Backend,
}

/// Stops a [`Watcher`] from another thread: [`Watcher::wait`] returns [`Wait::Stopped`].
#[derive(Clone)]
pub struct Waker {
    queue: Arc<Queue>,
}

impl Waker {
    pub fn stop(&self) {
        self.queue.stop();
    }
}

impl Watcher {
    /// Starts watching the directory trees at `roots` (directories that exist; nested roots
    /// are fine, their events are reported once per path after deduplication).
    pub fn new<P: AsRef<Path>>(roots: &[P], opts: WatchOptions) -> Result<Watcher, WatchError> {
        let roots: Vec<PathBuf> = roots.iter().map(|r| r.as_ref().to_path_buf()).collect();
        let queue = Arc::new(Queue::new(opts));
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        {
            let backend = backend::Backend::start(&roots, Arc::clone(&queue), &opts)?;
            Ok(Watcher { queue, backend })
        }
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        {
            let _ = (roots, queue);
            Err(WatchError::Unsupported)
        }
    }

    /// Waits for the next debounced batch and writes it to `out` (reusing its buffers).
    /// `timeout`: `None` waits until a batch arrives or the watcher is stopped.
    pub fn wait(&self, out: &mut Batch, timeout: Option<Duration>) -> Wait {
        self.queue.wait(out, timeout)
    }

    /// Delivers every event that happened before this call to the pending batch (FSEvents
    /// buffers events for a few milliseconds; inotify delivers at once). Tests and
    /// benchmarks use it to make "the edit is in the queue" deterministic.
    pub fn flush(&self) {
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        self.backend.flush();
    }

    /// A handle that stops this watcher from another thread.
    pub fn waker(&self) -> Waker {
        Waker { queue: Arc::clone(&self.queue) }
    }

    /// Whether the watcher was stopped (or its backend failed).
    pub fn stopped(&self) -> bool {
        self.queue.stopped()
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        self.queue.stop();
    }
}
