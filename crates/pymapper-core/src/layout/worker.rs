//! The layout on a background thread: lazy levels on request, published as shared slices.
//!
//! ```text
//!   GUI thread                         layout thread
//!   spawn(tree, cfg, cache dir) ─────> load cached levels (if the key matches) -> publish
//!   request(depth) ── want = max ────> while computed <= want: compute next level,
//!                                        publish (Arc, no copy), notify()
//!   level(d) / levels() <── Arc clones   idle with new levels: save the cache (atomic)
//!   drop ── stop ────────────────────> exit; joined
//! ```
//!
//! `notify` runs on the layout thread after every published level (and after loading the
//! cache): the GUI passes its repaint request. Readers never wait for a level being computed;
//! they draw what is there.

use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::JoinHandle;

use super::store::{cache_path, load_levels, save_levels};
use super::{Layout, LayoutConfig, LayoutTree, Rect, Region};

#[derive(Default)]
struct State {
    /// Deepest level requested.
    want: u32,
    levels: Vec<Arc<[Rect]>>,
    /// Levels loaded from the cache at startup.
    from_cache: u32,
    complete: bool,
    stop: bool,
}

struct Shared {
    st: Mutex<State>,
    cv: Condvar,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        // A panic on the layout thread aborts in release; in tests a poisoned lock still holds
        // consistent data (levels are pushed whole).
        self.st.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// A [`Layout`] computed on its own thread (see the module docs).
pub struct LayoutWorker {
    shared: Arc<Shared>,
    tree: Arc<LayoutTree>,
    cfg: LayoutConfig,
    regions: [Option<Rect>; 3],
    handle: Option<JoinHandle<()>>,
}

impl LayoutWorker {
    /// Starts the layout thread, which computes levels `0..=depth` right away. With
    /// `cache_dir`, levels cached for the same tree and config are loaded instead of computed,
    /// and newly computed levels are saved there when the thread is idle.
    pub fn spawn(
        tree: Arc<LayoutTree>,
        cfg: LayoutConfig,
        cache_dir: Option<PathBuf>,
        depth: u32,
        notify: Box<dyn Fn() + Send>,
    ) -> std::io::Result<LayoutWorker> {
        let layout = Layout::new(Arc::clone(&tree), cfg);
        let regions = [Region::Main, Region::ThirdParty, Region::Stdlib].map(|r| layout.region(r));
        let shared = Arc::new(Shared { st: Mutex::new(State { want: depth, complete: tree.levels() == 0, ..State::default() }), cv: Condvar::new() });
        let sh = Arc::clone(&shared);
        let handle = std::thread::Builder::new()
            .name("pymapper-layout".into())
            .spawn(move || run(&sh, layout, cache_dir, notify))?;
        Ok(LayoutWorker { shared, tree, cfg, regions, handle: Some(handle) })
    }

    #[inline]
    pub fn tree(&self) -> &Arc<LayoutTree> {
        &self.tree
    }

    #[inline]
    pub fn config(&self) -> &LayoutConfig {
        &self.cfg
    }

    /// Rect of a region (known before any level is computed).
    #[inline]
    pub fn region(&self, r: Region) -> Option<Rect> {
        self.regions[r as usize]
    }

    /// Asks for every level up to and including `depth` (requests only ever deepen).
    pub fn request(&self, depth: u32) {
        let mut st = self.shared.lock();
        if depth > st.want {
            st.want = depth;
            drop(st);
            self.shared.cv.notify_all();
        }
    }

    /// Number of published levels.
    pub fn computed(&self) -> u32 {
        self.shared.lock().levels.len() as u32
    }

    /// Levels that came from the on-disk cache.
    pub fn from_cache(&self) -> u32 {
        self.shared.lock().from_cache
    }

    /// Rects of level `d` by slot, if published.
    pub fn level(&self, d: u32) -> Option<Arc<[Rect]>> {
        self.shared.lock().levels.get(d as usize).cloned()
    }

    /// Every published level (cheap: `Arc` clones).
    pub fn levels(&self) -> Vec<Arc<[Rect]>> {
        self.shared.lock().levels.clone()
    }

    /// Requests `depth` and blocks until it is published or the tree has no such level;
    /// returns the number of published levels. For tests and batch callers; the GUI polls.
    pub fn wait_for(&self, depth: u32) -> u32 {
        self.request(depth);
        let mut st = self.shared.lock();
        while st.levels.len() as u32 <= depth && !st.complete && !st.stop {
            st = self.shared.cv.wait(st).unwrap_or_else(|e| e.into_inner());
        }
        st.levels.len() as u32
    }

    /// A [`Layout`] of the published levels (for synchronous use after waiting).
    pub fn snapshot(&self) -> Layout {
        Layout::with_levels(Arc::clone(&self.tree), self.cfg, self.levels())
    }
}

impl Drop for LayoutWorker {
    fn drop(&mut self) {
        self.shared.lock().stop = true;
        self.shared.cv.notify_all();
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

fn run(sh: &Shared, mut layout: Layout, cache_dir: Option<PathBuf>, notify: Box<dyn Fn() + Send>) {
    let path = cache_dir.map(|d| cache_path(&d, layout.tree(), layout.config()));
    if let Some(p) = &path
        && let Ok(levels) = load_levels(p, layout.tree(), layout.config())
        && !levels.is_empty()
    {
        layout = Layout::with_levels(Arc::clone(layout.tree()), *layout.config(), levels);
        let mut st = sh.lock();
        st.levels = layout.levels().to_vec();
        st.from_cache = layout.computed();
        st.complete = layout.is_complete();
        drop(st);
        sh.cv.notify_all();
        notify();
    }
    let mut saved = layout.computed();
    loop {
        {
            let mut st = sh.lock();
            loop {
                if st.stop {
                    return;
                }
                if layout.computed() <= st.want && !layout.is_complete() {
                    break;
                }
                if let Some(p) = &path
                    && saved < layout.computed()
                {
                    drop(st);
                    // Best effort: a failed write only costs a recompute next run.
                    let _ = save_levels(p, layout.tree(), layout.config(), layout.levels());
                    saved = layout.computed();
                    st = sh.lock();
                    continue;
                }
                st = sh.cv.wait(st).unwrap_or_else(|e| e.into_inner());
            }
        }
        let Some(lv) = layout.step().cloned() else { continue };
        let mut st = sh.lock();
        st.levels.push(lv);
        st.complete = layout.is_complete();
        drop(st);
        sh.cv.notify_all();
        notify();
    }
}
