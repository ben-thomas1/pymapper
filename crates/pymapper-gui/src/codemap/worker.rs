//! The code map's background thread: loads the analysis, builds the [`CodeMap`] for the
//! requested size metric and the [`Scene`] for the requested colour / edge options, and
//! publishes each result for the UI thread, which polls [`MapWorker::take`] once per frame
//! (one uncontended lock) and is woken by the `notify` hook (a repaint request).
//!
//! Requests coalesce: only the newest pending request is built. A new size metric rebuilds
//! the layout and edge table; other options only rebuild the scene.
//!
//! # Watch mode
//!
//! With [`LoadParams::watch`], a second thread (`pymapper-watch`) waits for debounced file
//! change batches and hands them to this one. Updating needs the analysis exclusively (the
//! incremental update patches the index in place, no copy), so the worker asks the UI to
//! let go of its code map first:
//!
//! ```text
//!   watch thread        map thread                                UI thread (about_to_wait)
//!   batch ──────────>   classify (shared read)
//!                       release wanted ─────────────────────────> drop CodeMap refs, views
//!                       wait until the code map is unique   <──── released
//!                       keep the old layout, unwrap the analysis
//!                       Analysis::update (or reload on config changes)
//!                       CodeMap::build, Transition::build, scene
//!                       publish { update: node map, transition } ─> remap selection, animate
//! ```
//!
//! The scene stays on screen meanwhile (it does not reference the analysis); only hover
//! details pause. Watch mode lays out in pre-order ([`Order::Preorder`]) so cells keep
//! their place when sizes change.

use std::path::Path;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use pymapper_core::index::NodeId;
use pymapper_core::layout::{Order, SizeMetric};
use pymapper_core::watch::{self, Batch, Changes, Wait, WatchOptions, Watcher, Waker};

use super::load::{self, Analysis, LoadParams};
use super::scene::{Legend, SceneParams};
use super::transition::Transition;
use super::{CodeMap, MapParams};
use crate::interact::sceneindex::SceneIndex;
use crate::map::scene::Scene;

/// What the UI wants shown.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Request {
    pub size: SizeMetric,
    pub scene: SceneParams,
}

impl Default for Request {
    fn default() -> Request {
        Request { size: SizeMetric::Loc, scene: SceneParams::default() }
    }
}

/// Progress of the worker.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Status {
    /// Loading; the phase name.
    Loading(&'static str),
    /// Building a layout or scene.
    Building,
    /// Applying file changes (watch mode); the map stays on screen.
    Updating,
    /// The newest request is published.
    Ready,
    /// Loading failed (configuration error); the message.
    Failed(String),
}

/// What a watch-mode update changed, published with the scene that shows it.
pub struct Update {
    /// New node of every node of the previous map (`NONE`: gone); empty after a reload
    /// (configuration change: nothing carries over).
    pub node_map: Vec<NodeId>,
    pub transition: Transition,
    /// Files the update re-extracted, added and removed.
    pub files: (u32, u32, u32),
    /// From the first file event of the batch to the published scene.
    pub latency: Duration,
    /// Waiting for the UI to release the old code map.
    pub release: Duration,
    /// Incremental update of the analysis (extract, patch, resolve, diff), or the reload.
    pub update: Duration,
    /// Rollup + aggregates, code map (layout, reveal bands, edges), transition and scene.
    pub rebuild: Duration,
}

/// A finished scene and what it was built from.
pub struct Published {
    pub map: Arc<CodeMap>,
    pub scene: Arc<Scene>,
    pub legend: Legend,
    /// Interaction lookup tables of `scene`.
    pub index: Arc<SceneIndex>,
    pub request: Request,
    /// Time to build the code map (when this request rebuilt it) and the scene.
    pub map_build: Option<Duration>,
    pub scene_build: Duration,
    /// Time to build `index`.
    pub index_build: Duration,
    /// Set when the scene shows a watch-mode update.
    pub update: Option<Update>,
}

/// The release handshake of a watch-mode update (see the module docs).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Release {
    #[default]
    None,
    /// The worker waits for the UI to drop its code map.
    Wanted,
    /// The UI dropped it.
    Done,
}

#[derive(Default)]
struct State {
    pending: Option<Request>,
    published: Option<Published>,
    status: Option<Status>,
    stop: bool,
    /// Change batches from the watch thread, oldest first.
    batches: Vec<Batch>,
    /// Consumed batches, returned to the watch thread (buffers reused).
    spare: Vec<Batch>,
    release: Release,
    /// Stops the watch thread.
    waker: Option<Waker>,
    /// The watcher could not start (the map still loads, without updates).
    watch_error: Option<String>,
}

struct Shared {
    state: Mutex<State>,
    wake: Condvar,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        // The state is plain data, valid after any panic: recover from poisoning.
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Handle of the background thread. Dropping it stops the thread after its current step
/// (without waiting for it).
pub struct MapWorker {
    shared: Arc<Shared>,
}

type Notify = Arc<dyn Fn() + Send + Sync>;

impl MapWorker {
    /// Starts the thread: loads `load`, then builds `first`. `notify` is called whenever
    /// the status changes or a result is published. With `load.watch`, the map is laid out
    /// in pre-order and file changes are applied as they happen.
    pub fn spawn(load: LoadParams, mut params: MapParams, first: Request, notify: Notify) -> std::io::Result<MapWorker> {
        if load.watch {
            params.layout.order = Order::Preorder;
        }
        let shared = Arc::new(Shared { state: Mutex::new(State { pending: Some(first), status: Some(Status::Loading("start")), ..State::default() }), wake: Condvar::new() });
        let s = Arc::clone(&shared);
        std::thread::Builder::new().name("pymapper-map".to_owned()).spawn(move || run(&s, &load, params, &notify))?;
        Ok(MapWorker { shared })
    }

    /// Asks for `req` (replaces any request not started yet).
    pub fn request(&self, req: Request) {
        let mut st = self.shared.lock();
        st.pending = Some(req);
        drop(st);
        self.shared.wake.notify_all();
    }

    /// The newest published result since the last call, if any.
    pub fn take(&self) -> Option<Published> {
        self.shared.lock().published.take()
    }

    /// Current status.
    pub fn status(&self) -> Status {
        self.shared.lock().status.clone().unwrap_or(Status::Ready)
    }

    /// Why the watcher could not start, if it failed (watch mode only).
    pub fn watch_error(&self) -> Option<String> {
        self.shared.lock().watch_error.clone()
    }

    /// The worker waits for the UI to drop every reference to the current code map (watch
    /// mode). Call [`MapWorker::released`] once done.
    pub fn wants_release(&self) -> bool {
        self.shared.lock().release == Release::Wanted
    }

    /// The UI dropped its code map references (see [`MapWorker::wants_release`]).
    pub fn released(&self) {
        let mut st = self.shared.lock();
        if st.release == Release::Wanted {
            st.release = Release::Done;
        }
        drop(st);
        self.shared.wake.notify_all();
    }

    /// Blocks until a result is published or loading fails (for benchmarks and tests).
    pub fn wait(&self) -> Result<Published, String> {
        self.wait_timeout(Duration::MAX).unwrap_or_else(|| Err("timed out".to_owned()))
    }

    /// [`MapWorker::wait`] with a timeout (`None`: nothing within `timeout`). Answers release
    /// requests itself, so tests and benchmarks without a UI see watch-mode updates.
    pub fn wait_timeout(&self, timeout: Duration) -> Option<Result<Published, String>> {
        let deadline = Instant::now().checked_add(timeout);
        let mut st = self.shared.lock();
        loop {
            if let Some(p) = st.published.take() {
                return Some(Ok(p));
            }
            if let Some(Status::Failed(e)) = &st.status {
                return Some(Err(e.clone()));
            }
            if st.release == Release::Wanted {
                st.release = Release::Done;
                self.shared.wake.notify_all();
            }
            let now = Instant::now();
            let wait = match deadline {
                Some(d) if d <= now => return None,
                Some(d) => (d - now).min(Duration::from_millis(50)),
                None => Duration::from_millis(50),
            };
            st = self.shared.wake.wait_timeout(st, wait).unwrap_or_else(PoisonError::into_inner).0;
        }
    }
}

impl Drop for MapWorker {
    fn drop(&mut self) {
        let mut st = self.shared.lock();
        st.stop = true;
        if let Some(w) = st.waker.take() {
            w.stop();
        }
        drop(st);
        self.shared.wake.notify_all();
    }
}

fn set_status(s: &Shared, status: Status, notify: &Notify) {
    s.lock().status = Some(status);
    s.wake.notify_all();
    notify();
}

/// Starts (or restarts) the watch thread on `root`. Failures are recorded in the state: the
/// map keeps working without updates.
fn start_watch(s: &Arc<Shared>, root: &Path, notify: &Notify) {
    let w = match Watcher::new(&[root], WatchOptions::default()) {
        Ok(w) => w,
        Err(e) => {
            s.lock().watch_error = Some(e.to_string());
            notify();
            return;
        }
    };
    let mut st = s.lock();
    if st.stop {
        return;
    }
    if let Some(old) = st.waker.replace(w.waker()) {
        old.stop();
    }
    st.watch_error = None;
    drop(st);
    let s2 = Arc::clone(s);
    let spawned = std::thread::Builder::new().name("pymapper-watch".to_owned()).spawn(move || watch_thread(&s2, &w));
    if let Err(e) = spawned {
        s.lock().watch_error = Some(format!("cannot start the watch thread: {e}"));
        notify();
    }
}

/// Hands each debounced batch to the map thread (buffers come back through `spare`).
fn watch_thread(s: &Shared, w: &Watcher) {
    let mut b = Batch::new();
    loop {
        match w.wait(&mut b, None) {
            Wait::Ready => {
                let mut st = s.lock();
                if st.stop {
                    return;
                }
                let next = st.spare.pop().unwrap_or_default();
                st.batches.push(std::mem::replace(&mut b, next));
                drop(st);
                s.wake.notify_all();
            }
            Wait::Timeout => {}
            Wait::Stopped => return,
        }
    }
}

enum Job {
    Request(Request),
    Batches(Vec<Batch>),
}

fn run(s: &Arc<Shared>, load: &LoadParams, params: MapParams, notify: &Notify) {
    let progress = |phase: &'static str| set_status(s, Status::Loading(phase), notify);
    let mut analysis = match load::load(load, &progress) {
        Ok(a) => Some(Arc::new(a)),
        Err(e) => {
            set_status(s, Status::Failed(format!("{e:#}")), notify);
            return;
        }
    };
    if load.watch
        && let Some(a) = &analysis
    {
        start_watch(s, &a.root, notify);
    }
    let mut map: Option<Arc<CodeMap>> = None;
    let mut last: Option<Request> = None;
    loop {
        let job = {
            let mut st = s.lock();
            loop {
                if st.stop {
                    return;
                }
                // Changes first (once there is a map to update), then the newest request.
                if last.is_some() && !st.batches.is_empty() {
                    // A pending request stays pending: it is built right after the update.
                    st.status = Some(Status::Updating);
                    break Job::Batches(std::mem::take(&mut st.batches));
                }
                if let Some(r) = st.pending.take() {
                    st.status = Some(Status::Building);
                    break Job::Request(r);
                }
                st = s.wake.wait(st).unwrap_or_else(PoisonError::into_inner);
            }
        };
        notify();
        match job {
            Job::Request(req) => {
                last = Some(req);
                // After a failed reload there is nothing to lay out until the next change.
                let Some(analysis) = &analysis else { continue };
                let mut map_build = None;
                let m = match &map {
                    Some(m) if m.params.size == req.size => Arc::clone(m),
                    _ => {
                        let t = Instant::now();
                        // Drop the old map first: two edge tables of a venv need not coexist.
                        drop(map.take());
                        let m = Arc::new(CodeMap::build(Arc::clone(analysis), MapParams { size: req.size, ..params }));
                        map_build = Some(t.elapsed());
                        map = Some(Arc::clone(&m));
                        m
                    }
                };
                let t = Instant::now();
                let (scene, legend, drawn) = m.scene(&req.scene);
                let scene_build = t.elapsed();
                let t = Instant::now();
                let index = Arc::new(SceneIndex::build(&scene, m.analysis.index().node_count(), &m.edges, &drawn));
                drop(drawn);
                let index_build = t.elapsed();
                let p = Published { map: m, scene: Arc::new(scene), legend, index, request: req, map_build, scene_build, index_build, update: None };
                publish(s, notify, p);
            }
            Job::Batches(batches) => {
                let Some(req) = last else { continue };
                if !apply_batches(s, load, params, notify, &mut analysis, &mut map, req, batches) {
                    return;
                }
            }
        }
    }
}

fn publish(s: &Shared, notify: &Notify, p: Published) {
    {
        let mut st = s.lock();
        st.published = Some(p);
        st.release = Release::None;
        if st.pending.is_none() && st.batches.is_empty() {
            st.status = Some(Status::Ready);
        }
    }
    s.wake.notify_all();
    notify();
}

/// The changes of every batch against the current index (`changed` sorted, unique).
/// Without an analysis (a failed reload), any change means loading again.
fn merge<'b>(batches: &'b [Batch], a: Option<&Analysis>) -> Changes<'b> {
    let mut out = Changes::default();
    let Some(a) = a else {
        out.reload = batches.iter().any(|b| !b.is_empty());
        return out;
    };
    for b in batches {
        let c = watch::classify(b, a.index(), &a.root);
        out.changed.extend(c.changed);
        out.rewalk |= c.rewalk;
        out.reload |= c.reload;
        out.all |= c.all;
        out.ignored += c.ignored;
    }
    out.changed.sort_unstable();
    out.changed.dedup();
    out
}

/// Waits until `arc` is the only reference (the UI and the views thread let go after the
/// release request), checking for a stop every couple of milliseconds. `None`: stopped.
fn unique<T>(s: &Shared, mut arc: Arc<T>) -> Option<T> {
    loop {
        match Arc::try_unwrap(arc) {
            Ok(v) => return Some(v),
            Err(a) => arc = a,
        }
        let st = s.lock();
        if st.stop {
            return None;
        }
        drop(s.wake.wait_timeout(st, Duration::from_millis(2)).unwrap_or_else(PoisonError::into_inner));
    }
}

/// Applies change batches: classify, release, update (or reload), rebuild, publish with the
/// transition. Returns false when the worker was stopped meanwhile.
#[allow(clippy::too_many_arguments)]
fn apply_batches(
    s: &Arc<Shared>,
    load: &LoadParams,
    params: MapParams,
    notify: &Notify,
    analysis: &mut Option<Arc<Analysis>>,
    map: &mut Option<Arc<CodeMap>>,
    req: Request,
    mut batches: Vec<Batch>,
) -> bool {
    let first = batches.iter().filter_map(Batch::first_event).min().unwrap_or_else(Instant::now);
    let changes = merge(&batches, analysis.as_deref());
    if changes.is_empty() {
        drop(changes);
        recycle(s, &mut batches);
        let mut st = s.lock();
        if st.pending.is_none() && st.batches.is_empty() {
            st.status = Some(Status::Ready);
        }
        drop(st);
        notify();
        return true;
    }

    // Release: the UI drops its references; the old layout is kept for the transition.
    let t = Instant::now();
    {
        let mut st = s.lock();
        st.published = None;
        st.release = Release::Wanted;
    }
    s.wake.notify_all();
    notify();
    let old_layout = match map.take() {
        Some(m) => match unique(s, m) {
            Some(cm) => Some(cm.layout),
            None => return false,
        },
        None => None,
    };
    let mut owned = match analysis.take() {
        Some(a) => match unique(s, a) {
            Some(a) => Some(a),
            None => return false,
        },
        None => None,
    };
    let release = t.elapsed();

    let t = Instant::now();
    let mut diff = None;
    let mut files = (0, 0, 0);
    let mut rollup = Duration::ZERO;
    let reload = changes.reload || owned.is_none();
    if !reload && let Some(a) = owned.as_mut() {
        let mut warnings = Vec::new();
        let (d, st, r) = a.update(&changes, &mut warnings);
        a.warnings.extend(warnings);
        files = (st.reextracted, st.added, st.removed);
        rollup = r;
        diff = Some(d);
    }
    drop(changes);
    recycle(s, &mut batches);
    let a = if reload {
        let old_root = owned.as_ref().map(|a| a.root.clone());
        // Free the old analysis before loading the new one (no two venv indexes at once).
        drop(owned);
        match load::load(load, &|_| {}) {
            Ok(new) => {
                if old_root.as_ref() != Some(&new.root) {
                    start_watch(s, &new.root, notify);
                }
                new
            }
            Err(e) => {
                // Nothing to show until the next change loads again (the UI keeps the last scene).
                s.lock().release = Release::None;
                set_status(s, Status::Failed(format!("{e:#}")), notify);
                return true;
            }
        }
    } else {
        owned.expect("an incremental update keeps its analysis")
    };
    let update = t.elapsed().saturating_sub(rollup);

    let t = Instant::now();
    let a = Arc::new(a);
    *analysis = Some(Arc::clone(&a));
    let m = Arc::new(CodeMap::build(a, MapParams { size: req.size, ..params }));
    let transition = match (&diff, &old_layout) {
        (Some(d), Some(old)) => Transition::build(old, &m.layout, d),
        _ => Transition::default(),
    };
    drop(old_layout);
    let ts = Instant::now();
    let (scene, legend, drawn) = m.scene(&req.scene);
    let scene_build = ts.elapsed();
    let ts = Instant::now();
    let index = Arc::new(SceneIndex::build(&scene, m.analysis.index().node_count(), &m.edges, &drawn));
    drop(drawn);
    let index_build = ts.elapsed();
    let rebuild = t.elapsed() + rollup;
    *map = Some(Arc::clone(&m));
    let node_map = diff.map(|d| d.node_map).unwrap_or_default();
    let latency = first.elapsed();
    let update = Update { node_map, transition, files, latency, release, update, rebuild };
    let p = Published { map: m, scene: Arc::new(scene), legend, index, request: req, map_build: None, scene_build, index_build, update: Some(update) };
    publish(s, notify, p);
    true
}

/// Returns consumed batches to the watch thread.
fn recycle(s: &Shared, batches: &mut Vec<Batch>) {
    let mut st = s.lock();
    st.spare.append(batches);
    // A handful of buffers covers any burst; drop the rest.
    st.spare.truncate(4);
}
