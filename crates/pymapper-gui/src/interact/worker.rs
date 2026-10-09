//! The interaction background thread: owns the core [`Query`] over the loaded analysis and
//! answers search, path, neighbour and source requests; a scoped helper thread runs the
//! findings pass once. The UI thread never waits: it posts jobs and polls results once per
//! frame (one uncontended lock); the worker wakes it through the `notify` hook.
//!
//! ```text
//!   spawn(Arc<Analysis>) ─> Query::new (call graph CSR) ─> SearchIndex (distinct names)
//!        ├─ scoped thread: findings::analyse ─────────────────────────> Results::insights
//!        └─ loop: newest job of each kind (search > neighbours > source > path > badges)
//! ```
//!
//! The query borrows the analysis inside the thread's frame (no self-referential struct, no
//! unsafe). Jobs coalesce per kind: only the newest pending one runs.

use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use pymapper_core::index::NodeId;
use pymapper_core::query::{PathGraph, PathOptions, Query, Scratch, ShortestPath, Walk, WalkOptions};

use super::findings::{self, Badges, Insights};
use super::search::{SearchHit, SearchIndex};
use super::source::{self, Highlighted};
use crate::codemap::load::Analysis;

/// Results returned per search.
pub const SEARCH_LIMIT: usize = 200;
/// Direct callers / callees listed per node.
pub const NEIGHBOUR_LIMIT: usize = 500;

/// A path request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PathJob {
    pub from: NodeId,
    pub to: NodeId,
    pub graph: PathGraph,
}

/// Search results for one query text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SearchResult {
    pub text: String,
    pub hits: Vec<SearchHit>,
    pub elapsed: Duration,
}

/// A path result (`path: None`: no path).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PathResult {
    pub job: PathJob,
    pub path: Option<ShortestPath>,
    pub elapsed: Duration,
}

/// Direct callers and callees of a node (its subtree for non-functions).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Neighbours {
    pub node: NodeId,
    pub callers: Walk,
    pub callees: Walk,
}

/// A source pane result.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceResult {
    pub node: NodeId,
    pub path: PathBuf,
    pub source: Result<Highlighted, String>,
    pub elapsed: Duration,
}

/// What the worker produced since the last [`InsightWorker::take`].
#[derive(Default)]
pub struct Results {
    pub search: Option<SearchResult>,
    pub path: Option<PathResult>,
    pub neighbours: Option<Neighbours>,
    pub source: Option<SourceResult>,
    pub insights: Option<Arc<Insights>>,
    pub badges: Option<Arc<Badges>>,
    /// Time to build the query state (call graph + search index), once.
    pub ready: Option<Duration>,
}

impl Results {
    /// Moves every result of `o` into `self` (newer ones replace older ones).
    pub fn merge(&mut self, o: Results) {
        macro_rules! take {
            ($($f:ident),*) => { $( if o.$f.is_some() { self.$f = o.$f; } )* };
        }
        take!(search, path, neighbours, source, insights, badges, ready);
    }

    pub fn is_empty(&self) -> bool {
        self.search.is_none()
            && self.path.is_none()
            && self.neighbours.is_none()
            && self.source.is_none()
            && self.insights.is_none()
            && self.badges.is_none()
            && self.ready.is_none()
    }
}

#[derive(Default)]
struct Jobs {
    search: Option<String>,
    neighbours: Option<NodeId>,
    source: Option<NodeId>,
    path: Option<PathJob>,
    badges: Option<u8>,
}

enum Job {
    Search(String),
    Neighbours(NodeId),
    Source(NodeId),
    Path(PathJob),
    Badges(u8),
}

impl Jobs {
    fn next(&mut self) -> Option<Job> {
        if let Some(s) = self.search.take() {
            return Some(Job::Search(s));
        }
        if let Some(v) = self.neighbours.take() {
            return Some(Job::Neighbours(v));
        }
        if let Some(v) = self.source.take() {
            return Some(Job::Source(v));
        }
        if let Some(p) = self.path.take() {
            return Some(Job::Path(p));
        }
        self.badges.take().map(Job::Badges)
    }
}

#[derive(Default)]
struct State {
    jobs: Jobs,
    results: Results,
    /// Insights are computed (badge jobs wait for them).
    insights: Option<Arc<Insights>>,
    stop: bool,
}

struct Shared {
    state: Mutex<State>,
    wake: Condvar,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

type Notify = Arc<dyn Fn() + Send + Sync>;

/// Handle of the interaction thread. Dropping it stops the thread after its current job.
pub struct InsightWorker {
    shared: Arc<Shared>,
}

impl InsightWorker {
    /// Starts the thread over `analysis`. `findings`: also run the findings pass.
    pub fn spawn(analysis: Arc<Analysis>, findings: bool, notify: Notify) -> std::io::Result<InsightWorker> {
        let shared = Arc::new(Shared { state: Mutex::new(State::default()), wake: Condvar::new() });
        let s = Arc::clone(&shared);
        std::thread::Builder::new().name("pymapper-interact".to_owned()).spawn(move || run(&s, &analysis, findings, &notify))?;
        Ok(InsightWorker { shared })
    }

    fn post(&self, f: impl FnOnce(&mut Jobs)) {
        f(&mut self.shared.lock().jobs);
        self.shared.wake.notify_all();
    }

    pub fn search(&self, text: &str) {
        self.post(|j| j.search = Some(text.to_owned()));
    }

    pub fn neighbours(&self, v: NodeId) {
        self.post(|j| j.neighbours = Some(v));
    }

    pub fn source(&self, v: NodeId) {
        self.post(|j| j.source = Some(v));
    }

    pub fn path(&self, job: PathJob) {
        self.post(|j| j.path = Some(job));
    }

    /// Recount the finding badges at `min_conf`.
    pub fn badges(&self, min_conf: u8) {
        self.post(|j| j.badges = Some(min_conf));
    }

    /// Everything produced since the last call.
    pub fn take(&self) -> Results {
        std::mem::take(&mut self.shared.lock().results)
    }

    /// Blocks until `pred` holds for `acc` plus everything produced since, merging new results
    /// into `acc` (benchmarks and tests).
    pub fn wait_into(&self, acc: &mut Results, pred: impl Fn(&Results) -> bool) {
        let mut st = self.shared.lock();
        loop {
            acc.merge(std::mem::take(&mut st.results));
            if pred(acc) {
                return;
            }
            st = self.shared.wake.wait(st).unwrap_or_else(PoisonError::into_inner);
        }
    }
}

impl Drop for InsightWorker {
    fn drop(&mut self) {
        self.shared.lock().stop = true;
        self.shared.wake.notify_all();
    }
}

fn publish(s: &Shared, notify: &Notify, f: impl FnOnce(&mut State)) {
    f(&mut s.lock());
    s.wake.notify_all();
    notify();
}

fn run(s: &Shared, analysis: &Analysis, with_findings: bool, notify: &Notify) {
    let t = Instant::now();
    let a = analysis;
    let q = Query::new(a.index(), a.imports(), a.calls());
    let si = SearchIndex::build(&q);
    let ready = t.elapsed();
    publish(s, notify, |st| st.results.ready = Some(ready));
    std::thread::scope(|scope| {
        if with_findings {
            let q = &q;
            scope.spawn(move || {
                let ins = Arc::new(findings::analyse(a, q));
                publish(s, notify, |st| {
                    st.results.insights = Some(Arc::clone(&ins));
                    st.insights = Some(ins);
                });
            });
        }
        serve(s, a, &q, &si, notify);
    });
}

fn serve(s: &Shared, a: &Analysis, q: &Query<'_>, si: &SearchIndex, notify: &Notify) {
    let mut scratch = Scratch::default();
    loop {
        let job = {
            let mut st = s.lock();
            loop {
                if st.stop {
                    return;
                }
                // Badge recounts wait for the insights.
                if st.insights.is_none() && st.jobs.search.is_none() && st.jobs.neighbours.is_none() && st.jobs.source.is_none() && st.jobs.path.is_none() {
                    st = s.wake.wait(st).unwrap_or_else(PoisonError::into_inner);
                    continue;
                }
                if let Some(j) = st.jobs.next() {
                    break j;
                }
                st = s.wake.wait(st).unwrap_or_else(PoisonError::into_inner);
            }
        };
        let t = Instant::now();
        match job {
            Job::Search(text) => {
                let hits = si.search(q, &text, SEARCH_LIMIT);
                let r = SearchResult { text, hits, elapsed: t.elapsed() };
                publish(s, notify, |st| st.results.search = Some(r));
            }
            Job::Neighbours(v) => {
                let opts = WalkOptions { limit: NEIGHBOUR_LIMIT, ..WalkOptions::default() };
                let callers = q.callers(v, &opts, &mut scratch);
                let callees = q.callees(v, &opts, &mut scratch);
                publish(s, notify, |st| st.results.neighbours = Some(Neighbours { node: v, callers, callees }));
            }
            Job::Source(v) => {
                let r = read_source(a, v, t);
                publish(s, notify, |st| st.results.source = Some(r));
            }
            Job::Path(job) => {
                let opts = PathOptions { graph: job.graph, ..PathOptions::default() };
                let path = q.path(job.from, job.to, &opts, &mut scratch);
                let r = PathResult { job, path, elapsed: t.elapsed() };
                publish(s, notify, |st| st.results.path = Some(r));
            }
            Job::Badges(min_conf) => {
                let ins = s.lock().insights.clone();
                if let Some(ins) = ins {
                    let b = Arc::new(ins.badges_at(a.index(), min_conf));
                    publish(s, notify, |st| st.results.badges = Some(b));
                }
            }
        }
    }
}

/// The source of node `v`: its def span (a module: the whole file, capped), highlighted.
fn read_source(a: &Analysis, v: NodeId, t: Instant) -> SourceResult {
    let ix = a.index();
    let (Some(f), Some(r)) = (ix.file(v), ix.def_row(v)) else {
        return SourceResult { node: v, path: PathBuf::new(), source: Err("no source file (distribution or namespace package)".to_owned()), elapsed: t.elapsed() };
    };
    let defs = &ix.facts().defs;
    let path = ix.file_path(f).to_path_buf();
    let source = source::load(&path, defs.start[r], defs.end[r], defs.start_line[r]).map_err(|e| e.to_string());
    SourceResult { node: v, path, source, elapsed: t.elapsed() }
}
