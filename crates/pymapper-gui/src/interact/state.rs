//! UI-thread interaction state: what is hovered, selected, searched, the path, the overlay
//! toggles, the latest worker results, and when the two overlay scenes must be rebuilt.
//! No egui here (see [`super::panel`]), so every transition is unit-tested.
//!
//! ```text
//!   published scene ──set_scene──> overlays dirty, selection kept (same analysis)
//!   canvas hover    ──set_hover──> highlight dirty (only when the focus changes)
//!   click / search  ──select─────> neighbours + source requested, highlight dirty
//!   shift-click     ──path_to────> path requested; result -> highlight dirty
//!   worker results  ──poll───────> search hits, neighbours, source, path, insights
//!   per frame       ──sync_overlays(map)──> rebuilds only the dirty overlay scenes
//! ```

use std::sync::Arc;
use std::time::{Duration, Instant};

use pymapper_core::index::NodeId;
use pymapper_core::query::PathGraph;

use super::findings::{Badges, Insights};
use super::highlight::{self, Base, Focus, HighlightSpec, HighlightStats};
use super::marks::{self, MarksSpec, MarksStats};
use super::sceneindex::SceneIndex;
use super::worker::{InsightWorker, Neighbours, PathJob, PathResult, Results, SearchResult, SourceResult};
use crate::codemap::CodeMap;
use crate::map::scene::{Scene, WorldRect};
use crate::map::{MapState, OVERLAY_HIGHLIGHT, OVERLAY_MARKS, RebuildStats};

/// Fraction of the canvas a focused cell fills.
pub const FOCUS_FILL: f32 = 0.6;

/// The scene the interaction works on.
#[derive(Clone)]
pub struct SceneCtx {
    pub map: Arc<CodeMap>,
    pub scene: Arc<Scene>,
    pub index: Arc<SceneIndex>,
}

/// See the module docs.
pub struct Interaction {
    pub ctx: Option<SceneCtx>,
    pub worker: Option<InsightWorker>,
    /// Error starting the worker thread.
    pub worker_error: Option<String>,
    pub hover: Option<Focus>,
    pub selected: Option<NodeId>,
    /// Qualified id of the selected node (one string per selection).
    pub selected_qid: String,
    /// Highlight the hovered item's neighbourhood.
    pub hover_highlight: bool,
    /// Dim the map under a highlight.
    pub dim: bool,
    // Search.
    pub search_text: String,
    sent_search: String,
    pub search: Option<SearchResult>,
    // Path.
    pub path_graph: PathGraph,
    pub path: Option<PathResult>,
    path_pending: bool,
    // Worker results.
    pub neighbours: Option<Neighbours>,
    pub source: Option<SourceResult>,
    /// Bumped per source result (a re-read of the same node invalidates its layout).
    pub source_gen: u64,
    pub insights: Option<Arc<Insights>>,
    pub badges: Option<Arc<Badges>>,
    /// Time to build the query state, once ready.
    pub query_ready: Option<Duration>,
    // Overlays.
    pub marks: MarksSpec,
    /// The user moved the confidence slider (otherwise it follows the configured
    /// `min-confidence` of each analysis).
    min_conf_user: bool,
    /// `MapState::scene_generation` the overlays were last built for (a scene swap clears
    /// them, e.g. returning from another view).
    overlay_scene_gen: Option<u64>,
    pub marks_stats: MarksStats,
    pub highlight_stats: HighlightStats,
    highlight_dirty: bool,
    marks_dirty: bool,
    /// Camera request: fit this world rect (set by focus, consumed by the app).
    pub focus_request: Option<WorldRect>,
    /// Overlay rebuild timings.
    pub highlight_builds: RebuildStats,
    pub marks_builds: RebuildStats,
}

impl Default for Interaction {
    fn default() -> Self {
        Interaction {
            ctx: None,
            worker: None,
            worker_error: None,
            hover: None,
            selected: None,
            selected_qid: String::new(),
            hover_highlight: true,
            dim: true,
            search_text: String::new(),
            sent_search: String::new(),
            search: None,
            path_graph: PathGraph::Calls,
            path: None,
            path_pending: false,
            neighbours: None,
            source: None,
            source_gen: 0,
            insights: None,
            badges: None,
            query_ready: None,
            marks: MarksSpec::default(),
            min_conf_user: false,
            overlay_scene_gen: None,
            marks_stats: MarksStats::default(),
            highlight_stats: HighlightStats::default(),
            highlight_dirty: false,
            marks_dirty: false,
            focus_request: None,
            highlight_builds: RebuildStats::default(),
            marks_builds: RebuildStats::default(),
        }
    }
}

impl Interaction {
    /// A new scene was published. The first one (or one of another analysis) starts the
    /// worker; `notify` wakes the UI thread when the worker has results.
    pub fn set_scene(&mut self, ctx: SceneCtx, findings: bool, notify: Arc<dyn Fn() + Send + Sync>) {
        let same = self.ctx.as_ref().is_some_and(|c| Arc::ptr_eq(&c.map.analysis, &ctx.map.analysis));
        if !same {
            self.worker = None;
            match InsightWorker::spawn(Arc::clone(&ctx.map.analysis), findings, notify) {
                Ok(w) => self.worker = Some(w),
                Err(e) => self.worker_error = Some(format!("cannot start the interaction thread: {e}")),
            }
            self.reset_results();
            // Findings and badges start at the configured threshold, as `pymapper check`.
            if !self.min_conf_user {
                self.marks.min_conf = ctx.map.analysis.config.min_confidence;
            }
        }
        self.ctx = Some(ctx);
        // Hover ids refer to the old scene's edges.
        self.hover = None;
        self.highlight_dirty = true;
        self.marks_dirty = true;
    }

    fn reset_results(&mut self) {
        self.selected = None;
        self.selected_qid.clear();
        self.sent_search.clear();
        self.search = None;
        self.path = None;
        self.neighbours = None;
        self.source = None;
        self.insights = None;
        self.badges = None;
        self.query_ready = None;
    }

    /// Takes the worker's results. Returns true if anything changed.
    pub fn poll(&mut self) -> bool {
        let Some(w) = &self.worker else { return false };
        let r = w.take();
        self.apply(r)
    }

    /// Applies worker results (separate from [`Self::poll`] for tests).
    pub fn apply(&mut self, r: Results) -> bool {
        if r.is_empty() {
            return false;
        }
        if let Some(s) = r.search {
            self.search = Some(s);
        }
        if let Some(n) = r.neighbours
            && Some(n.node) == self.selected
        {
            self.neighbours = Some(n);
        }
        if let Some(s) = r.source
            && Some(s.node) == self.selected
        {
            self.source = Some(s);
            self.source_gen += 1;
        }
        if let Some(p) = r.path {
            self.path_pending = false;
            // A path from a node no longer selected (cleared or changed meanwhile) is stale.
            if Some(p.job.from) == self.selected {
                self.path = Some(p);
                self.highlight_dirty = true;
            }
        }
        if let Some(i) = r.insights {
            if !self.min_conf_user {
                self.marks.min_conf = i.badges.min_conf;
            } else if self.marks.badges
                && i.badges.min_conf != self.marks.min_conf
                && let Some(w) = &self.worker
            {
                // The slider moved before the findings were ready.
                w.badges(self.marks.min_conf);
            }
            self.badges = Some(Arc::clone(&i.badges));
            self.insights = Some(i);
            self.marks_dirty = true;
        }
        if let Some(b) = r.badges {
            if self.marks.badges
                && b.min_conf != self.marks.min_conf
                && let Some(w) = &self.worker
            {
                // The slider moved back while this recount ran.
                w.badges(self.marks.min_conf);
            }
            self.badges = Some(b);
            self.marks_dirty = true;
        }
        if let Some(t) = r.ready {
            self.query_ready = Some(t);
            // A search typed while the query state was being built.
            self.sent_search.clear();
        }
        true
    }

    /// Sends the search text to the worker if it changed since the last send.
    pub fn update_search(&mut self) {
        if self.search_text == self.sent_search {
            return;
        }
        if let Some(w) = &self.worker {
            w.search(&self.search_text);
            self.sent_search.clone_from(&self.search_text);
        }
        if self.search_text.trim().is_empty() {
            self.search = None;
        }
    }

    /// The pointer is over `f` (`None`: nothing). Returns true if the highlight must change.
    pub fn set_hover(&mut self, f: Option<Focus>) -> bool {
        if self.hover == f {
            return false;
        }
        self.hover = f;
        if self.hover_highlight {
            self.highlight_dirty = true;
        }
        self.hover_highlight
    }

    /// Selects `v` (`None`: clears the selection and the path). Requests its neighbours and
    /// source.
    pub fn select(&mut self, v: Option<NodeId>) {
        if self.selected == v {
            return;
        }
        self.selected = v;
        self.neighbours = None;
        self.source = None;
        self.selected_qid.clear();
        self.highlight_dirty = true;
        let Some(v) = v else {
            self.path = None;
            return;
        };
        if let Some(c) = &self.ctx {
            let ix = &c.map.analysis.index;
            if v < ix.node_count() {
                let mut q = Vec::new();
                ix.qid(v, &mut q);
                self.selected_qid = String::from_utf8_lossy(&q).into_owned();
            }
        }
        if let Some(w) = &self.worker {
            w.neighbours(v);
            w.source(v);
        }
    }

    /// Selects `v` and asks the camera to frame its cell (or its nearest drawn ancestor's).
    pub fn focus(&mut self, v: NodeId) {
        self.select(Some(v));
        self.focus_request = self.cell_rect(v);
    }

    /// World rect of `v`'s cell or its nearest drawn ancestor's.
    pub fn cell_rect(&self, v: NodeId) -> Option<WorldRect> {
        let c = self.ctx.as_ref()?;
        let base = Base { scene: &c.scene, sx: &c.index, edges: &c.map.edges, ix: &c.map.analysis.index };
        if v >= base.ix.node_count() {
            return None;
        }
        let cell = base.cell_or_ancestor(v)?;
        let r = &c.scene.rects.inst[cell as usize];
        Some(WorldRect { min: r.min, max: r.max })
    }

    /// Requests the shortest path from the selected node to `to` (calls or imports, as
    /// [`Self::path_graph`]). Without a selection, selects `to` instead.
    pub fn path_to(&mut self, to: NodeId) {
        let Some(from) = self.selected else {
            self.select(Some(to));
            return;
        };
        self.request_path(from, to);
    }

    pub fn request_path(&mut self, from: NodeId, to: NodeId) {
        if let Some(w) = &self.worker {
            w.path(PathJob { from, to, graph: self.path_graph });
            self.path_pending = true;
        }
    }

    pub fn path_pending(&self) -> bool {
        self.path_pending
    }

    pub fn clear_path(&mut self) {
        if self.path.take().is_some() {
            self.highlight_dirty = true;
        }
    }

    /// The overlay toggles changed.
    pub fn set_marks(&mut self, m: MarksSpec) {
        if m == self.marks {
            return;
        }
        self.min_conf_user |= m.min_conf != self.marks.min_conf;
        self.marks = m;
        self.marks_dirty = true;
        // Recount whenever the badges shown would be at another threshold, including when
        // they are turned on after the slider moved (before the findings are ready, the
        // insights' arrival does it).
        if m.badges
            && self.badges.as_ref().is_some_and(|b| b.min_conf != m.min_conf)
            && let Some(w) = &self.worker
        {
            w.badges(m.min_conf);
        }
    }

    /// Marks the highlight for a rebuild (e.g. the dim toggle changed).
    pub fn invalidate_highlight(&mut self) {
        self.highlight_dirty = true;
    }

    pub fn needs_overlays(&self) -> bool {
        self.highlight_dirty || self.marks_dirty
    }

    /// Rebuilds the dirty overlay scenes into `map`. Returns true if one changed.
    pub fn sync_overlays(&mut self, map: &mut MapState) -> bool {
        let Some(c) = self.ctx.clone() else { return false };
        // Only overlays of the scene shown (a scene published but not yet set is ignored).
        if !Arc::ptr_eq(&c.scene, &map.scene) {
            return false;
        }
        if self.overlay_scene_gen != Some(map.scene_generation) {
            self.overlay_scene_gen = Some(map.scene_generation);
            self.highlight_dirty = true;
            self.marks_dirty = true;
        }
        let base = Base { scene: &c.scene, sx: &c.index, edges: &c.map.edges, ix: &c.map.analysis.index };
        let mut changed = false;
        if self.highlight_dirty {
            self.highlight_dirty = false;
            let t = Instant::now();
            let path = self.path.as_ref().and_then(|p| p.path.as_ref()).map_or(&[][..], |p| &p.nodes[..]);
            let hover = if self.hover_highlight { self.hover } else { None };
            let spec = HighlightSpec { hover, selected: self.selected, path, dim: self.dim, ..HighlightSpec::default() };
            let (scene, stats) = highlight::build(&base, &spec);
            self.highlight_stats = stats;
            map.set_overlay(OVERLAY_HIGHLIGHT, Arc::new(scene));
            self.highlight_builds.record(t.elapsed());
            changed = true;
        }
        if self.marks_dirty {
            self.marks_dirty = false;
            let t = Instant::now();
            let scene = match (&self.insights, &self.badges) {
                (Some(ins), Some(b)) if self.marks.any() => {
                    let (s, st) = marks::build(&base, &c.map, ins, b, &self.marks);
                    self.marks_stats = st;
                    s
                }
                _ => {
                    self.marks_stats = MarksStats::default();
                    Scene::default()
                }
            };
            map.set_overlay(OVERLAY_MARKS, Arc::new(scene));
            self.marks_builds.record(t.elapsed());
            changed = true;
        }
        changed
    }
}
