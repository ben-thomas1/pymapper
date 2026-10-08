//! The code-map canvas renderer: instanced rects (SDF rounded), lines and Bezier curves
//! (ribbons tessellated in the vertex shader), glyph-atlas labels with LOD, a camera
//! uniform, and an own spatial grid for culling and hit testing.
//!
//! ```text
//! scene.rs   instance structs, LOD bands, SceneBuilder -> Scene (sorted by layer, bin)
//! grid.rs    pyramid of uniform grids: bins, query -> bin ranges, range limiting
//! band.rs    band-filtered slots + per-bin slot tables (rebuilt only on LOD change)
//! camera.rs  pan/zoom, world <-> screen, zoom limits
//! hit.rs     exact hit tests over the grid's candidates
//! text.rs    fonts (glyphon/cosmic-text), own glyph atlas packer, label layout
//! gpu.rs     wgpu pipelines and buffers; uploads only when band/labels change
//! stress.rs  synthetic stress scene + camera path for measurements
//! map.wgsl   all shaders (shared camera uniform)
//! ```
//!
//! Frame flow: [`MapState::update`] (CPU; rebuilds band slots or labels only when the band
//! or label region changed) -> [`gpu::MapRenderer::prepare`] (uploads only what changed,
//! writes the 64-byte camera uniform, computes culled draw ranges from the grid) ->
//! [`gpu::MapRenderer::draw`]. Panning and zooming within a band re-run only the range
//! computation; no instance buffer is touched.

pub mod band;
pub mod camera;
pub mod gpu;
pub mod grid;
pub mod hit;
pub mod scene;
pub mod stress;
pub mod text;

#[cfg(test)]
mod tests;

use std::sync::Arc;
use std::time::{Duration, Instant};

use band::QueryScratch;
use camera::Camera;
use hit::{Bands, Hit, HitKind, HitScratch};
use scene::{Band, GlyphInstance, Rgba, Scene, WorldRect, band_zoom};
use text::TextSystem;

/// Label font size in points (scaled by the DPI factor).
pub const LABEL_POINTS: f32 = 11.0;
/// Label inset from the rect's top-left corner, in points.
pub const LABEL_PAD_POINTS: f32 = 3.0;
/// A label is only laid out for rects that can be at least this wide (pixels) in the band.
pub const MIN_LABEL_PX: f32 = 24.0;
/// Glyphs are not laid out beyond this label width (pixels).
pub const MAX_LABEL_PX: f32 = 640.0;
/// At most this many labels per rebuild; the largest rects win.
pub const MAX_LABELS: usize = 16_384;
/// Initial glyph atlas size (texels per side); grows by doubling when full.
pub const ATLAS_SIZE: u32 = 1024;
/// Hit-test tolerance around edges, in points.
pub const HIT_TOLERANCE_POINTS: f32 = 3.0;

/// Pointer and keyboard input for the canvas, gathered by the panel layout each frame.
/// Positions are in points relative to the canvas top-left.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct CanvasInput {
    pub hover: Option<[f32; 2]>,
    /// Drag delta in points (content follows the pointer).
    pub drag: [f32; 2],
    /// Multiplicative zoom factor (1.0 = none) about `zoom_anchor` (or the canvas centre).
    pub zoom: f32,
    pub zoom_anchor: Option<[f32; 2]>,
    /// Fit the whole scene (Home key / double click).
    pub fit: bool,
}

impl CanvasInput {
    pub const NONE: Self = Self { hover: None, drag: [0.0, 0.0], zoom: 1.0, zoom_anchor: None, fit: false };
}

/// Duration counters for one kind of rebuild.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RebuildStats {
    pub count: u64,
    pub total: Duration,
    pub max: Duration,
}

impl RebuildStats {
    pub fn record(&mut self, d: Duration) {
        self.count += 1;
        self.total += d;
        self.max = self.max.max(d);
    }
    pub fn mean(&self) -> Duration {
        if self.count == 0 { Duration::ZERO } else { self.total / self.count as u32 }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MapStats {
    pub band_rebuilds: RebuildStats,
    pub label_rebuilds: RebuildStats,
    /// Instance buffer uploads after band rebuilds (CPU time to fill wgpu's staging memory).
    pub band_uploads: RebuildStats,
    /// Bytes written by the last band upload.
    pub band_upload_bytes: u64,
    /// Instances in the last frame's draw ranges (after grid culling).
    pub drawn_instances: u64,
    pub draw_calls: u32,
    /// Labels and glyphs in the current label set.
    pub labels: u32,
    pub glyphs: u32,
}

/// Current label set: glyph instances for the label region at one band and font size.
#[derive(Debug, Default)]
pub struct Labels {
    pub glyphs: Vec<GlyphInstance>,
    pub region: WorldRect,
    pub band: Option<Band>,
    pub px: f32,
    /// Bumped on every rebuild (the renderer re-uploads when it changes).
    pub generation: u64,
    candidates: Vec<(f32, u32)>,
}

type HoverKey = (Option<[f32; 2]>, Camera, u64, u64, f32);

/// CPU-side state of the map: scene, camera, band-filtered views, labels, hover.
pub struct MapState {
    pub scene: Arc<Scene>,
    pub camera: Camera,
    pub bands: Bands,
    /// Bumped whenever some batch's band contents change.
    pub band_generation: u64,
    /// Bumped on [`Self::set_scene`].
    pub scene_generation: u64,
    pub labels: Labels,
    pub text: Option<TextSystem>,
    pub hover: Option<Hit>,
    pub stats: MapStats,
    /// Pixels per point of the last update (label size, hit tolerance).
    pub pixels_per_point: f32,
    /// When set, every item is drawn regardless of LOD (stress worst case).
    pub force_all: bool,
    needs_fit: bool,
    /// Inputs of the last hover hit test: (pointer, camera, band and scene generation, ppp).
    hover_key: Option<HoverKey>,
    query: QueryScratch,
    hit_scratch: HitScratch,
    ranges: Vec<std::ops::Range<u32>>,
}

impl std::fmt::Debug for MapState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MapState").field("camera", &self.camera).field("stats", &self.stats).finish_non_exhaustive()
    }
}

impl Default for MapState {
    fn default() -> Self {
        Self::new(Arc::new(Scene::default()))
    }
}

impl MapState {
    pub fn new(scene: Arc<Scene>) -> Self {
        let mut s = Self {
            scene: Arc::new(Scene::default()),
            camera: Camera::default(),
            bands: Bands::default(),
            band_generation: 0,
            scene_generation: 0,
            labels: Labels::default(),
            text: None,
            hover: None,
            stats: MapStats::default(),
            pixels_per_point: 1.0,
            force_all: false,
            needs_fit: true,
            hover_key: None,
            query: QueryScratch::default(),
            hit_scratch: HitScratch::default(),
            ranges: Vec::new(),
        };
        s.set_scene(scene);
        s
    }

    /// Replaces the scene; the camera fits it on the next update.
    pub fn set_scene(&mut self, scene: Arc<Scene>) {
        self.scene = scene;
        self.bands = Bands::default();
        self.labels.band = None;
        self.hover = None;
        self.scene_generation += 1;
        self.needs_fit = true;
    }

    /// Band used for filtering (the camera's, or the top band when forcing everything).
    pub fn effective_band(&self) -> Band {
        if self.force_all { scene::MAX_BAND } else { self.camera.band() }
    }

    /// Per-frame CPU update for a canvas of `viewport` physical pixels. Rebuilds band
    /// slots and labels only when needed. Returns true if anything was rebuilt.
    pub fn update(&mut self, viewport: [f32; 2], pixels_per_point: f32) -> bool {
        self.camera.set_viewport(viewport[0], viewport[1]);
        if pixels_per_point.is_finite() && pixels_per_point > 0.0 {
            self.pixels_per_point = pixels_per_point;
        }
        if self.needs_fit && self.scene.bounds.is_positive() {
            self.camera.set_limits(&self.scene.bounds);
            self.camera.fit(&self.scene.bounds, 0.95);
            self.needs_fit = false;
        }
        let mut rebuilt = false;
        // Each batch is rebuilt (and re-uploaded) only if its own membership changes.
        let band = if self.force_all { None } else { Some(self.camera.band()) };
        let t = Instant::now();
        let scene = &*self.scene;
        let Bands { rects, lines, curves } = &mut self.bands;
        // The batches are independent: filter them concurrently.
        let ((r, l), c) = rayon::join(
            || (rects.refresh(&scene.rects, band), lines.refresh(&scene.lines, band)),
            || curves.refresh(&scene.curves, band),
        );
        if r || l || c {
            self.band_generation += 1;
            self.stats.band_rebuilds.record(t.elapsed());
            rebuilt = true;
        }
        let px = (LABEL_POINTS * self.pixels_per_point).round();
        let view = self.camera.visible();
        let lb = Some(self.effective_band());
        if self.labels.band != lb || self.labels.px != px || !self.labels.region.contains_rect(&view) {
            let t = Instant::now();
            self.rebuild_labels(px);
            self.stats.label_rebuilds.record(t.elapsed());
            rebuilt = true;
        }
        rebuilt
    }

    /// Lays out the labels for the current band around the current view.
    pub fn rebuild_labels(&mut self, px: f32) {
        let band = self.effective_band();
        let text = self.text.get_or_insert_with(|| TextSystem::new(px, ATLAS_SIZE));
        text.set_px(px);
        let view = self.camera.visible();
        let region = view.expand(view.width(), view.height());
        let pad = (LABEL_PAD_POINTS * self.pixels_per_point).round();
        // The largest zoom inside this band decides whether a label can ever show here.
        let max_zoom = if self.force_all { self.camera.zoom } else { band_zoom(band.saturating_add(1)) as f32 };
        let min_w = MIN_LABEL_PX / max_zoom;
        let min_h = (text.line_height() + 2.0 * pad) / max_zoom;
        let labels = &mut self.labels;
        labels.glyphs.clear();
        labels.candidates.clear();
        let scene = &*self.scene;
        for li in 0..scene.rects.layers.len() {
            self.bands.rects.query(&scene.rects, li, &region, usize::MAX, &mut self.query, &mut self.ranges);
            for r in &self.ranges {
                for slot in r.clone() {
                    let i = self.bands.rects.slots[slot as usize];
                    if scene.rect_label[i as usize].len == 0 {
                        continue;
                    }
                    let inst = &scene.rects.inst[i as usize];
                    let rect = WorldRect { min: inst.min, max: inst.max };
                    if rect.width() < min_w || rect.height() < min_h || !rect.intersects(&region) {
                        continue;
                    }
                    labels.candidates.push((rect.width() * rect.height(), i));
                }
            }
        }
        if labels.candidates.len() > MAX_LABELS {
            labels.candidates.select_nth_unstable_by(MAX_LABELS, |a, b| b.0.total_cmp(&a.0));
            labels.candidates.truncate(MAX_LABELS);
        }
        // Draw order follows the batch (layer) order: parents' labels under children's.
        labels.candidates.sort_unstable_by_key(|c| c.1);
        for &(_, i) in &labels.candidates {
            let inst = &scene.rects.inst[i as usize];
            let rect = WorldRect { min: inst.min, max: inst.max };
            let max_px = (rect.width() * max_zoom - 2.0 * pad).min(MAX_LABEL_PX);
            let color = label_color(inst.fill);
            text.layout(scene.label(i as usize), rect, color, max_px, &mut labels.glyphs);
        }
        labels.region = region;
        labels.band = Some(band);
        labels.px = px;
        labels.generation += 1;
        self.stats.labels = labels.candidates.len() as u32;
        self.stats.glyphs = labels.glyphs.len() as u32;
    }

    /// Applies pointer input (pan/zoom/fit) and updates the hover hit. Returns true if
    /// the hovered item changed.
    pub fn apply_input(&mut self, input: &CanvasInput, pixels_per_point: f32) -> bool {
        let ppp = if pixels_per_point.is_finite() && pixels_per_point > 0.0 { pixels_per_point } else { 1.0 };
        if input.fit && self.scene.bounds.is_positive() {
            self.camera.fit(&self.scene.bounds, 0.95);
        }
        self.camera.pan_px(input.drag[0] * ppp, input.drag[1] * ppp);
        if input.zoom != 1.0 {
            let anchor = input.zoom_anchor.map_or([self.camera.viewport[0] * 0.5, self.camera.viewport[1] * 0.5], |a| [a[0] * ppp, a[1] * ppp]);
            self.camera.zoom_at(anchor, input.zoom);
        }
        // Hit test only when something that affects the answer changed (frames are also
        // drawn for unrelated reasons, e.g. panel animations).
        let key = (input.hover, self.camera, self.band_generation, self.scene_generation, ppp);
        if self.hover_key == Some(key) {
            return false;
        }
        self.hover_key = Some(key);
        let hit = input.hover.and_then(|h| self.hit_test([h[0] * ppp, h[1] * ppp], HIT_TOLERANCE_POINTS * ppp));
        let changed = hit != self.hover;
        self.hover = hit;
        changed
    }

    /// Topmost item at canvas pixel `screen` among what the current band draws. Uses the
    /// band slots of the last [`Self::update`].
    pub fn hit_test(&mut self, screen: [f32; 2], tol_px: f32) -> Option<Hit> {
        hit::hit_test(&self.scene, &self.bands, &self.camera, screen, tol_px, self.pixels_per_point, &mut self.hit_scratch)
    }

    /// A one-line description of the hovered item, written into `out` (reused).
    pub fn describe_hover(&self, out: &mut String) {
        use std::fmt::Write as _;
        out.clear();
        let Some(h) = self.hover else { return };
        match h.kind {
            HitKind::Rect => {
                let label = String::from_utf8_lossy(self.scene.label(h.index as usize));
                let _ = write!(out, "rect #{} {}", h.id, label);
            }
            HitKind::Line => {
                let _ = write!(out, "line #{}", h.id);
            }
            HitKind::Curve => {
                let _ = write!(out, "edge #{}", h.id);
            }
        }
    }
}

/// Dark text on light fills, light text on dark fills.
pub fn label_color(fill: Rgba) -> Rgba {
    let [r, g, b, _] = fill.0;
    let lum = 0.299 * f32::from(r) + 0.587 * f32::from(g) + 0.114 * f32::from(b);
    if lum > 150.0 { Rgba::rgb(20, 22, 26) } else { Rgba::rgb(235, 238, 242) }
}
