//! The code map's background thread: loads the analysis, builds the [`CodeMap`] for the
//! requested size metric and the [`Scene`] for the requested colour / edge options, and
//! publishes each result for the UI thread, which polls [`MapWorker::take`] once per frame
//! (one uncontended lock) and is woken by the `notify` hook (a repaint request).
//!
//! Requests coalesce: only the newest pending request is built. A new size metric rebuilds
//! the layout and edge table; other options only rebuild the scene.

use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use pymapper_core::layout::SizeMetric;

use super::load::{self, LoadParams};
use super::scene::{Legend, SceneParams};
use super::{CodeMap, MapParams};
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
    /// The newest request is published.
    Ready,
    /// Loading failed (configuration error); the message.
    Failed(String),
}

/// A finished scene and what it was built from.
pub struct Published {
    pub map: Arc<CodeMap>,
    pub scene: Arc<Scene>,
    pub legend: Legend,
    pub request: Request,
    /// Time to build the code map (when this request rebuilt it) and the scene.
    pub map_build: Option<Duration>,
    pub scene_build: Duration,
}

#[derive(Default)]
struct State {
    pending: Option<Request>,
    published: Option<Published>,
    status: Option<Status>,
    stop: bool,
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
    /// the status changes or a result is published.
    pub fn spawn(load: LoadParams, params: MapParams, first: Request, notify: Notify) -> std::io::Result<MapWorker> {
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
        self.shared.wake.notify_one();
    }

    /// The newest published result since the last call, if any.
    pub fn take(&self) -> Option<Published> {
        self.shared.lock().published.take()
    }

    /// Current status.
    pub fn status(&self) -> Status {
        self.shared.lock().status.clone().unwrap_or(Status::Ready)
    }

    /// Blocks until a result is published or loading fails (for benchmarks and tests).
    pub fn wait(&self) -> Result<Published, String> {
        let mut st = self.shared.lock();
        loop {
            if let Some(p) = st.published.take() {
                return Ok(p);
            }
            if let Some(Status::Failed(e)) = &st.status {
                return Err(e.clone());
            }
            st = self.shared.wake.wait(st).unwrap_or_else(PoisonError::into_inner);
        }
    }
}

impl Drop for MapWorker {
    fn drop(&mut self) {
        self.shared.lock().stop = true;
        self.shared.wake.notify_all();
    }
}

fn set_status(s: &Shared, status: Status, notify: &Notify) {
    s.lock().status = Some(status);
    s.wake.notify_all();
    notify();
}

fn run(s: &Shared, load: &LoadParams, params: MapParams, notify: &Notify) {
    let progress = |phase: &'static str| set_status(s, Status::Loading(phase), notify);
    let analysis = match load::load(load, &progress) {
        Ok(a) => Arc::new(a),
        Err(e) => {
            set_status(s, Status::Failed(format!("{e:#}")), notify);
            return;
        }
    };
    let mut map: Option<Arc<CodeMap>> = None;
    loop {
        let req = {
            let mut st = s.lock();
            loop {
                if st.stop {
                    return;
                }
                if let Some(r) = st.pending.take() {
                    st.status = Some(Status::Building);
                    break r;
                }
                st = s.wake.wait(st).unwrap_or_else(PoisonError::into_inner);
            }
        };
        notify();
        let mut map_build = None;
        let m = match &map {
            Some(m) if m.params.size == req.size => Arc::clone(m),
            _ => {
                let t = Instant::now();
                // Drop the old map first: two edge tables of a venv need not coexist.
                drop(map.take());
                let m = Arc::new(CodeMap::build(Arc::clone(&analysis), MapParams { size: req.size, ..params }));
                map_build = Some(t.elapsed());
                map = Some(Arc::clone(&m));
                m
            }
        };
        let t = Instant::now();
        let (scene, legend, _) = m.scene(&req.scene);
        let scene_build = t.elapsed();
        {
            let mut st = s.lock();
            st.published = Some(Published { map: m, scene: Arc::new(scene), legend, request: req, map_build, scene_build });
            if st.pending.is_none() {
                st.status = Some(Status::Ready);
            }
        }
        s.wake.notify_all();
        notify();
    }
}
