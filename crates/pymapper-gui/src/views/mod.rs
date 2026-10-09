//! Secondary views of the analysed project, linked to the code map by a shared selection:
//!
//! - Graph: force-directed node-link diagram of the code map's *current level* (the cells
//!   shown at the camera's band inside the camera's view), laid out by Barnes-Hut on the
//!   views' background thread and shown progressively;
//! - Matrix: dependency structure matrix of packages and modules, ordered so a layered
//!   system is lower triangular and cycles form blocks on the diagonal;
//! - Focus: the N-hop neighbourhood of the selection (call graph for classes and functions,
//!   module graph otherwise), laid out radially.
//!
//! ```text
//!   quadtree.rs  own Barnes-Hut quadtree (flat arrays, rebuilt per iteration)
//!   force.rs     force-directed layout (ForceAtlas2 forces, cooling), stepped
//!   graph.rs     view graphs: current level (cut + view), N-hop focus, adjacency, radial
//!   dsm.rs       matrix: SCC condensation, sink-first Kahn order, Eades-Lin-Smyth in cycles
//!   scene.rs     node-link and matrix scenes for the map renderer, selection outlines
//!   worker.rs    background thread: requests -> (progressive) scenes
//!   ui.rs        egui: view tabs, per-view options, texts
//!   text.rs      hover details, panel texts, selection outlines
//! ```
//!
//! All views share the canvas renderer and one [`MapState`]: switching views swaps the scene
//! and restores the view's own camera. Selection is one [`NodeId`]: a click in any view sets
//! it; every view outlines it (or its deepest ancestor it shows) in the overlay pass, without
//! a scene rebuild; the code map frames it when shown again after a change elsewhere.

pub mod dsm;
pub mod force;
pub mod graph;
pub mod quadtree;
pub mod scene;
mod text;
pub mod ui;
pub mod worker;

#[cfg(test)]
mod tests;

use std::sync::Arc;
use std::time::Duration;

use pymapper_core::index::NodeId;

use crate::codemap::CodeMap;
use crate::codemap::metrics::ColorMetric;
use crate::map::MapState;
use crate::map::camera::Camera;
use crate::map::hit::{Hit, HitKind};
use crate::map::scene::{Band, Scene, WorldRect};
use dsm::DsmParams;
use graph::{FocusParams, Weight};
use ui::ViewsUi;
use worker::{Content, GraphParams, Published, Request, ViewWorker};

/// Returning to the code map moves the camera to a selection made elsewhere if its cell is
/// off screen or smaller than this (physical pixels).
pub const REFRAME_MIN_PX: f32 = 24.0;

/// The views (tabs over the canvas).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum ViewKind {
    #[default]
    CodeMap,
    Graph,
    Matrix,
    Focus,
}

impl ViewKind {
    pub const ALL: [ViewKind; 4] = [ViewKind::CodeMap, ViewKind::Graph, ViewKind::Matrix, ViewKind::Focus];

    pub fn label(self) -> &'static str {
        match self {
            ViewKind::CodeMap => "Code map",
            ViewKind::Graph => "Graph",
            ViewKind::Matrix => "Matrix",
            ViewKind::Focus => "Focus",
        }
    }

    /// Slot of a secondary view.
    fn slot(self) -> Option<usize> {
        match self {
            ViewKind::CodeMap => None,
            ViewKind::Graph => Some(0),
            ViewKind::Matrix => Some(1),
            ViewKind::Focus => Some(2),
        }
    }
}

/// Matrix options as edited in the panel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MatrixOptions {
    pub weight: Weight,
    pub external: bool,
    /// Scope the matrix to the selection's subtree.
    pub scope_to_selection: bool,
}

impl Default for MatrixOptions {
    fn default() -> MatrixOptions {
        MatrixOptions { weight: Weight::All, external: false, scope_to_selection: false }
    }
}

/// Options of the secondary views.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ViewOptions {
    pub graph: GraphParams,
    pub matrix: MatrixOptions,
    pub focus: FocusParams,
}

/// What a view was (or is being) built from; the map is compared by pointer. The address
/// cannot be reused by another map while a key holding it is live: the pending request (in
/// the worker) or the published result (in the slot) keeps that map alive.
#[derive(Clone, Debug, PartialEq)]
enum Key {
    Graph { map: usize, band: Band, view: WorldRect, params: GraphParams },
    Matrix { map: usize, params: DsmParams },
    Focus { map: usize, node: NodeId, params: FocusParams, color: ColorMetric },
}

/// One secondary view: its newest result and its camera while hidden.
#[derive(Default)]
struct Slot {
    key: Option<Key>,
    /// Sequence number of the newest request.
    seq: u64,
    /// Request whose result `scene` / `content` hold.
    scene_seq: u64,
    scene: Option<Arc<Scene>>,
    content: Option<Content>,
    map: Option<Arc<CodeMap>>,
    camera: Option<Camera>,
    done: bool,
    iters: u32,
    elapsed: Duration,
    /// Request whose scene the canvas shows (its later snapshots keep the camera).
    shown_seq: u64,
}

type Notify = Arc<dyn Fn() + Send + Sync>;

/// The views controller (UI thread): view switching, requests, results, selection, marks.
pub struct Views {
    pub active: ViewKind,
    pub selection: Option<NodeId>,
    pub options: ViewOptions,
    /// Outline the selection on the code map too (off when the interaction's highlight
    /// overlay draws it).
    pub codemap_marks: bool,
    notify: Notify,
    worker: Option<ViewWorker>,
    slots: [Slot; 3],
    /// The code map's scene and camera while another view is shown.
    codemap_scene: Option<Arc<Scene>>,
    codemap_camera: Option<Camera>,
    /// The code map camera's band and view when it was last shown (the graph view's input).
    codemap_band: Band,
    codemap_view: WorldRect,
    /// The selection changed while the code map was hidden: frame it when shown.
    reframe: bool,
    /// Subtree the matrix is scoped to ("Only modules under the selection").
    matrix_scope: Option<NodeId>,
    /// Texts and marks need refreshing.
    dirty: bool,
    error: Option<String>,
}

impl Views {
    /// `notify` wakes the UI thread (egui repaint request); the worker thread starts on first
    /// use of a secondary view.
    pub fn new(notify: Notify) -> Views {
        Views {
            active: ViewKind::CodeMap,
            selection: None,
            options: ViewOptions::default(),
            codemap_marks: true,
            notify,
            worker: None,
            slots: Default::default(),
            codemap_scene: None,
            codemap_camera: None,
            codemap_band: 0,
            codemap_view: WorldRect::new(f32::NEG_INFINITY, f32::NEG_INFINITY, f32::INFINITY, f32::INFINITY),
            reframe: false,
            matrix_scope: None,
            dirty: true,
            error: None,
        }
    }

    /// Shows a newly published code map scene, or keeps it for later while another view is
    /// shown.
    pub fn show_codemap_scene(&mut self, map: &mut MapState, scene: Arc<Scene>) {
        if self.active == ViewKind::CodeMap {
            if map.scene.bounds.is_positive() {
                map.replace_scene(scene);
            } else {
                map.set_scene(scene);
            }
        } else {
            self.codemap_scene = Some(scene);
        }
        self.dirty = true;
    }

    /// The secondary-view worker is building something.
    pub fn busy(&self) -> bool {
        self.worker.as_ref().is_some_and(ViewWorker::busy)
    }

    /// Per frame, after the egui pass: tab switches, option changes, worker results, texts
    /// and selection marks. `color` is the code map's colour metric (the node-link views use
    /// it too). Returns true if the panel texts changed (they show from the next frame on).
    pub fn frame(&mut self, ui: &mut ViewsUi, map: &mut MapState, codemap: Option<&Arc<CodeMap>>, color: ColorMetric) -> bool {
        ui.enabled = codemap.is_some();
        if ui.focus_selection {
            ui.focus_selection = false;
            self.switch(ViewKind::Focus, map, codemap, color);
        }
        if let Some(k) = ui.tab.take() {
            self.switch(k, map, codemap, color);
        }
        if ui.options_changed {
            ui.options_changed = false;
            // The matrix scope is the selection at the moment the box is ticked.
            if ui.options.matrix.scope_to_selection && !self.options.matrix.scope_to_selection {
                self.matrix_scope = self.selection;
            } else if !ui.options.matrix.scope_to_selection {
                self.matrix_scope = None;
            }
            self.options = ui.options;
            self.ensure(map, codemap, color);
            self.dirty = true;
        }
        self.poll(map);
        ui.active = self.active;
        if !self.dirty {
            return false;
        }
        self.dirty = false;
        self.refresh_texts(ui, codemap);
        self.update_marks(map, codemap);
        true
    }

    /// Takes the newest worker result and shows it if its view is active.
    fn poll(&mut self, map: &mut MapState) {
        let Some(p) = self.worker.as_ref().and_then(ViewWorker::take) else { return };
        let Published { seq, scene, content, map: cm, done, iters, elapsed } = p;
        let Some(i) = self.slots.iter().position(|s| s.seq == seq) else { return };
        let slot = &mut self.slots[i];
        slot.scene_seq = seq;
        slot.scene = Some(Arc::clone(&scene));
        slot.content = Some(content);
        slot.map = Some(cm);
        slot.done = done;
        slot.iters = iters;
        slot.elapsed = elapsed;
        if self.active.slot() == Some(i) {
            if slot.shown_seq == seq {
                map.replace_scene(scene);
            } else {
                map.show_scene(scene, None);
                slot.shown_seq = seq;
            }
        }
        self.dirty = true;
    }

    /// Shows view `to`: saves the current view's camera, requests what `to` needs, and swaps
    /// in its scene (with its own camera).
    pub fn switch(&mut self, to: ViewKind, map: &mut MapState, codemap: Option<&Arc<CodeMap>>, color: ColorMetric) {
        if to == self.active {
            if to == ViewKind::Focus {
                self.ensure(map, codemap, color);
            }
            return;
        }
        match self.active.slot() {
            None => {
                self.codemap_camera = map.fitted_camera();
                if let Some(c) = self.codemap_camera {
                    self.codemap_band = c.band();
                    self.codemap_view = c.visible();
                }
                self.codemap_scene = Some(Arc::clone(&map.scene));
            }
            Some(i) => self.slots[i].camera = map.fitted_camera(),
        }
        self.active = to;
        self.dirty = true;
        match to.slot() {
            None => {
                let scene = self.codemap_scene.take().unwrap_or_default();
                map.show_scene(scene, self.codemap_camera);
                if std::mem::take(&mut self.reframe)
                    && let (Some(cm), Some(v)) = (codemap, self.selection)
                    && let Some(m) = scene::codemap_mark(cm, v)
                {
                    // Move only if the cell is off screen or too small to see.
                    let r = WorldRect { min: m.min, max: m.max };
                    let px = r.width().min(r.height()) * map.camera.zoom;
                    if map.fitted_camera().is_none() || !map.camera.visible().contains_rect(&r) || px < REFRAME_MIN_PX {
                        map.frame(&r, 0.4);
                    }
                }
            }
            Some(i) => {
                self.ensure(map, codemap, color);
                let slot = &mut self.slots[i];
                match &slot.scene {
                    Some(s) => {
                        // A stale result (newer request pending) is framed afresh.
                        let camera = if slot.scene_seq == slot.seq { slot.camera } else { None };
                        map.show_scene(Arc::clone(s), camera);
                        slot.shown_seq = slot.scene_seq;
                    }
                    None => map.show_scene(Arc::new(Scene::default()), None),
                }
            }
        }
    }

    /// The key of the active secondary view for the current inputs (`None`: nothing to build).
    fn key(&self, codemap: &Arc<CodeMap>, color: ColorMetric) -> Option<Key> {
        let map = Arc::as_ptr(codemap) as usize;
        match self.active {
            ViewKind::CodeMap => None,
            ViewKind::Graph => Some(Key::Graph { map, band: self.codemap_band, view: self.codemap_view, params: GraphParams { color, ..self.options.graph } }),
            ViewKind::Matrix => {
                let o = self.options.matrix;
                let scope = if o.scope_to_selection { self.matrix_scope } else { None };
                Some(Key::Matrix { map, params: DsmParams { weight: o.weight, external: o.external, scope } })
            }
            ViewKind::Focus => self.selection.map(|node| Key::Focus { map, node, params: self.options.focus, color }),
        }
    }

    /// Requests the active secondary view if its inputs changed (or an interrupted force
    /// layout should resume).
    fn ensure(&mut self, map: &mut MapState, codemap: Option<&Arc<CodeMap>>, color: ColorMetric) {
        let (Some(i), Some(cm)) = (self.active.slot(), codemap) else { return };
        let key = self.key(cm, color);
        let slot = &self.slots[i];
        let resume = slot.key.is_some() && slot.key == key && !slot.done && self.active == ViewKind::Graph;
        if slot.key == key && !resume {
            return;
        }
        let Some(k) = key else {
            // Focus without a selection: nothing to show.
            self.slots[i] = Slot::default();
            map.show_scene(Arc::new(Scene::default()), None);
            self.dirty = true;
            return;
        };
        if self.worker.is_none() {
            match ViewWorker::spawn(Arc::clone(&self.notify)) {
                Ok(w) => self.worker = Some(w),
                Err(e) => {
                    self.error = Some(format!("cannot start the views thread: {e}"));
                    self.dirty = true;
                    return;
                }
            }
        }
        let Some(w) = self.worker.as_ref() else { return };
        let req = match &k {
            Key::Graph { band, view, params, .. } => Request::Graph { map: Arc::clone(cm), band: *band, view: *view, params: *params },
            Key::Matrix { params, .. } => Request::Matrix { map: Arc::clone(cm), params: *params },
            Key::Focus { node, params, color, .. } => Request::Focus { map: Arc::clone(cm), node: *node, params: *params, color: *color },
        };
        let seq = w.request(req);
        let slot = &mut self.slots[i];
        if resume {
            // Same content under a new number: later snapshots keep the camera.
            if slot.scene_seq == slot.seq {
                slot.scene_seq = seq;
            }
            if slot.shown_seq == slot.seq {
                slot.shown_seq = seq;
            }
        } else {
            // New content: its first result is framed, not shown through the old camera.
            slot.camera = None;
            slot.shown_seq = 0;
        }
        slot.seq = seq;
        slot.key = Some(k);
        slot.done = false;
        self.dirty = true;
    }

    /// A primary click on the canvas with `hit` under the pointer: selects what was clicked
    /// (empty space clears the selection). In the focus view, clicking a node refocuses on it.
    pub fn on_click(&mut self, hit: Option<Hit>, map: &mut MapState, codemap: Option<&Arc<CodeMap>>, color: ColorMetric) {
        let sel = match hit {
            None => None,
            Some(h) => match self.hit_node(h) {
                Some(v) => Some(v),
                None => return, // an edge or a matrix block: keep the selection
            },
        };
        if sel == self.selection {
            return;
        }
        self.selection = sel;
        self.reframe = self.active != ViewKind::CodeMap;
        self.dirty = true;
        if self.active == ViewKind::Focus && sel.is_some() {
            self.ensure(map, codemap, color);
        }
    }

    /// Sets the selection from outside the views (the code map's interaction: click, search,
    /// findings). Unlike [`Self::on_click`] it never reframes or refocuses.
    pub fn set_selection(&mut self, sel: Option<NodeId>) {
        if sel != self.selection {
            self.selection = sel;
            self.dirty = true;
        }
    }

    /// The index node behind a rect hit in the active view (`None`: edges, blocks).
    fn hit_node(&self, h: Hit) -> Option<NodeId> {
        if h.kind != HitKind::Rect {
            return None;
        }
        let Some(i) = self.active.slot() else { return Some(crate::codemap::scene::rect_node(h.id)) };
        match self.slots[i].content.as_ref()? {
            Content::NodeLink { graph, .. } => graph.nodes.get(h.id as usize).copied(),
            Content::Matrix { dsm, .. } => {
                let (kind, idx) = scene::matrix_item(h.id);
                match kind {
                    scene::ID_CELL => dsm.cells.get(idx as usize).map(|c| dsm.nodes[c.row as usize]),
                    scene::ID_ROW | scene::ID_COL => dsm.nodes.get(idx as usize).copied(),
                    _ => None,
                }
            }
        }
    }
}
