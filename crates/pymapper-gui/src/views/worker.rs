//! The secondary views' background thread. Builds the graph (progressively, one force
//! iteration at a time), matrix and focus views on request and publishes each scene for the UI
//! thread, which polls [`ViewWorker::take`] once per frame (one uncontended lock) and is woken
//! through the `notify` hook (a repaint request).
//!
//! - Requests coalesce: only the newest pending request is built.
//! - A force layout checks for a newer request between iterations and yields to it, keeping its
//!   state: asking for the same graph again resumes where it stopped.
//! - Progressive snapshots: the start layout (treemap centres: stable geography) at once,
//!   then one scene at most every [`SNAPSHOT_EVERY`], and the converged one.
//! - The call and module adjacencies of the focus view are built on first use and kept per
//!   analysis.

use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use pymapper_core::index::NodeId;

use super::dsm::{self, Dsm, DsmParams};
use super::force::{ForceGraph, ForceLayout, ForceParams};
use super::graph::{Adjacency, FocusParams, ViewGraph, Weight, focus_graph, focus_uses_calls, level_graph, radial_layout};
use super::scene::{self, MatrixGeom, NodeLink, NodeLinkStyle};
use crate::codemap::CodeMap;
use crate::codemap::metrics::ColorMetric;
use crate::map::scene::{Band, Scene, WorldRect};

/// Minimum time between two progressive force snapshots.
pub const SNAPSHOT_EVERY: Duration = Duration::from_millis(50);

/// Options of the force-directed graph view.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GraphParams {
    /// Node budget (the largest cells win).
    pub max_nodes: usize,
    pub weight: Weight,
    pub color: ColorMetric,
    pub force: ForceParams,
}

impl Default for GraphParams {
    fn default() -> GraphParams {
        GraphParams { max_nodes: 3000, weight: Weight::All, color: ColorMetric::Kind, force: ForceParams::default() }
    }
}

/// What to build.
#[derive(Clone)]
pub enum Request {
    /// The code map's current level: cells shown at `band` that intersect `view`.
    Graph { map: Arc<CodeMap>, band: Band, view: WorldRect, params: GraphParams },
    Matrix { map: Arc<CodeMap>, params: DsmParams },
    Focus { map: Arc<CodeMap>, node: NodeId, params: FocusParams, color: ColorMetric },
}

impl std::fmt::Debug for Request {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Request::Graph { band, view, params, .. } => f.debug_struct("Graph").field("band", band).field("view", view).field("params", params).finish(),
            Request::Matrix { params, .. } => f.debug_struct("Matrix").field("params", params).finish(),
            Request::Focus { node, params, .. } => f.debug_struct("Focus").field("node", node).field("params", params).finish(),
        }
    }
}

/// A view's content as drawn.
#[derive(Clone, Debug)]
pub enum Content {
    NodeLink {
        graph: Arc<ViewGraph>,
        nl: NodeLink,
        /// Local id of the focus node (focus view).
        focus: Option<u32>,
        weight: Weight,
        /// The focus walked the call graph (else the module graph).
        calls: bool,
    },
    Matrix {
        dsm: Arc<Dsm>,
        geom: MatrixGeom,
        /// `(node, row)` sorted by node.
        rows: Arc<[(NodeId, u32)]>,
    },
}

/// A finished (or progressive) scene.
pub struct Published {
    /// Sequence number of the request it answers.
    pub seq: u64,
    pub scene: Arc<Scene>,
    pub content: Content,
    /// The code map it was cut from.
    pub map: Arc<CodeMap>,
    /// Final: no further snapshot follows for this request.
    pub done: bool,
    /// Force iterations so far (0 for the other views).
    pub iters: u32,
    /// Wall time spent on this request so far (graph cut + layout + scenes).
    pub elapsed: Duration,
}

#[derive(Default)]
struct State {
    pending: Option<(u64, Request)>,
    published: Option<Published>,
    next_seq: u64,
    busy: bool,
    stop: bool,
}

struct Shared {
    state: Mutex<State>,
    wake: Condvar,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        // Plain data, valid after any panic: recover from poisoning.
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn has_pending(&self) -> bool {
        let st = self.lock();
        st.pending.is_some() || st.stop
    }
}

type Notify = Arc<dyn Fn() + Send + Sync>;

/// Handle of the background thread; dropping it stops the thread after its current step.
pub struct ViewWorker {
    shared: Arc<Shared>,
}

impl ViewWorker {
    pub fn spawn(notify: Notify) -> std::io::Result<ViewWorker> {
        let shared = Arc::new(Shared { state: Mutex::new(State::default()), wake: Condvar::new() });
        let s = Arc::clone(&shared);
        std::thread::Builder::new().name("pymapper-views".to_owned()).spawn(move || run(&s, &notify))?;
        Ok(ViewWorker { shared })
    }

    /// Asks for `req` (replacing any request not started yet); returns its sequence number.
    pub fn request(&self, req: Request) -> u64 {
        let mut st = self.shared.lock();
        st.next_seq += 1;
        let seq = st.next_seq;
        st.pending = Some((seq, req));
        drop(st);
        self.shared.wake.notify_all();
        seq
    }

    /// The newest result since the last call.
    pub fn take(&self) -> Option<Published> {
        self.shared.lock().published.take()
    }

    /// A request is pending or being built.
    pub fn busy(&self) -> bool {
        let st = self.shared.lock();
        st.busy || st.pending.is_some()
    }

    /// Blocks until a result is published (benchmarks and tests).
    pub fn wait(&self) -> Published {
        let mut st = self.shared.lock();
        loop {
            if let Some(p) = st.published.take() {
                return p;
            }
            st = self.shared.wake.wait(st).unwrap_or_else(PoisonError::into_inner);
        }
    }

    /// Blocks until the final result of request `seq` (skipping progressive snapshots).
    pub fn wait_done(&self, seq: u64) -> Published {
        loop {
            let p = self.wait();
            if p.seq == seq && p.done {
                return p;
            }
        }
    }
}

impl Drop for ViewWorker {
    fn drop(&mut self) {
        self.shared.lock().stop = true;
        self.shared.wake.notify_all();
    }
}

/// A force layout in progress (kept when a newer request interrupts it).
struct ForceRun {
    map: Arc<CodeMap>,
    band: Band,
    view: WorldRect,
    params: GraphParams,
    graph: Arc<ViewGraph>,
    fg: ForceGraph,
    layout: ForceLayout,
    radii: Vec<f32>,
    elapsed: Duration,
}

impl ForceRun {
    fn same(&self, map: &Arc<CodeMap>, band: Band, view: &WorldRect, params: &GraphParams) -> bool {
        Arc::ptr_eq(&self.map, map) && self.band == band && self.view == *view && self.params == *params
    }
}

/// Adjacencies of the focus view, per analysis.
#[derive(Default)]
struct Adjacencies {
    key: usize,
    calls: Option<Arc<Adjacency>>,
    modules: Option<Arc<Adjacency>>,
}

fn publish(s: &Shared, notify: &Notify, p: Published) {
    let done = p.done;
    {
        let mut st = s.lock();
        st.published = Some(p);
        if done && st.pending.is_none() {
            st.busy = false;
        }
    }
    s.wake.notify_all();
    notify();
}

fn run(s: &Shared, notify: &Notify) {
    let mut force: Option<ForceRun> = None;
    let mut adj = Adjacencies::default();
    loop {
        let (seq, req) = {
            let mut st = s.lock();
            loop {
                if st.stop {
                    return;
                }
                if let Some(r) = st.pending.take() {
                    st.busy = true;
                    break r;
                }
                st.busy = false;
                st = s.wake.wait(st).unwrap_or_else(PoisonError::into_inner);
            }
        };
        match req {
            Request::Graph { map, band, view, params } => {
                let resume = force.take().filter(|f| f.same(&map, band, &view, &params));
                let mut f = resume.unwrap_or_else(|| start_force(map, band, view, params));
                run_force(s, notify, seq, &mut f);
                force = Some(f);
            }
            Request::Matrix { map, params } => {
                let t = Instant::now();
                let d = dsm::build(&map, params);
                let (sc, geom) = scene::matrix(&map, &d);
                let rows: Arc<[(NodeId, u32)]> = d.row_index().into();
                let content = Content::Matrix { dsm: Arc::new(d), geom, rows };
                publish(s, notify, Published { seq, scene: Arc::new(sc), content, map, done: true, iters: 0, elapsed: t.elapsed() });
            }
            Request::Focus { map, node, params, color } => {
                let t = Instant::now();
                let key = Arc::as_ptr(&map.analysis) as usize;
                if adj.key != key {
                    adj = Adjacencies { key, ..Adjacencies::default() };
                }
                let ix = &map.analysis.index;
                let calls = node < ix.node_count() && focus_uses_calls(ix, node);
                let a = if calls {
                    Arc::clone(adj.calls.get_or_insert_with(|| Arc::new(Adjacency::calls(ix.node_count(), &map.analysis.rollup))))
                } else {
                    Arc::clone(adj.modules.get_or_insert_with(|| Arc::new(Adjacency::modules(ix, &map.analysis.rollup))))
                };
                let g = if node < ix.node_count() { focus_graph(&a, node, params) } else { ViewGraph::default() };
                let pos = radial_layout(&g);
                let r = scene::radii(&map, &g);
                let focus = g.local(node);
                let style = NodeLinkStyle { color, weight: Weight::Sites, focus };
                let (sc, nl) = scene::node_link(&map, &g, &pos, &r, &style);
                let content = Content::NodeLink { graph: Arc::new(g), nl, focus, weight: Weight::Sites, calls };
                publish(s, notify, Published { seq, scene: Arc::new(sc), content, map, done: true, iters: 0, elapsed: t.elapsed() });
            }
        }
    }
}

/// Cuts the graph and sets up its layout from the treemap centres.
fn start_force(map: Arc<CodeMap>, band: Band, view: WorldRect, params: GraphParams) -> ForceRun {
    let t = Instant::now();
    let mut g = level_graph(&map, band, &view, params.max_nodes);
    g.edges.retain(|e| e.weight(params.weight) > 0);
    let max = g.edges.iter().map(|e| e.weight(params.weight)).max().unwrap_or(1);
    let log_max = (1.0 + max as f32).ln().max(f32::MIN_POSITIVE);
    let fg = ForceGraph { n: g.nodes.len() as u32, edges: g.edges.iter().map(|e| (e.src, e.dst, ((1.0 + e.weight(params.weight) as f32).ln() / log_max).max(0.05))).collect() };
    let init: Vec<[f32; 2]> = g.nodes.iter().map(|&v| map.layout.rect(v).map_or([0.0; 2], |r| [(r.x0 + r.x1) * 0.5, (r.y0 + r.y1) * 0.5])).collect();
    let layout = ForceLayout::new(&fg, &init, params.force);
    let radii = scene::radii(&map, &g);
    ForceRun { map, band, view, params, graph: Arc::new(g), fg, layout, radii, elapsed: t.elapsed() }
}

/// Iterates `f` until it converges or a newer request arrives, publishing snapshots.
fn run_force(s: &Shared, notify: &Notify, seq: u64, f: &mut ForceRun) {
    let mut t = Instant::now();
    let mut last: Option<Instant> = None;
    loop {
        let done = f.layout.done();
        if done || last.is_none_or(|l| l.elapsed() >= SNAPSHOT_EVERY) {
            let style = NodeLinkStyle { color: f.params.color, weight: f.params.weight, focus: None };
            let (sc, nl) = scene::node_link(&f.map, &f.graph, &f.layout.pos, &f.radii, &style);
            let content = Content::NodeLink { graph: Arc::clone(&f.graph), nl, focus: None, weight: f.params.weight, calls: false };
            let now = Instant::now();
            f.elapsed += now - t;
            t = now;
            publish(s, notify, Published { seq, scene: Arc::new(sc), content, map: Arc::clone(&f.map), done, iters: f.layout.iter, elapsed: f.elapsed });
            last = Some(Instant::now());
        }
        if done || s.has_pending() {
            break;
        }
        f.layout.step(&f.fg);
    }
    f.elapsed += t.elapsed();
}
