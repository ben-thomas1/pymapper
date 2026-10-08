//! Scene data: GPU instance structs, level-of-detail bands and the scene builder.
//!
//! A [`Scene`] is immutable once built. Each item kind lives in a [`Batch`]: instances in
//! GPU layout, sorted by (layer, grid bin) so that a band-filtered copy keeps the order the
//! spatial grid needs (see [`super::grid`]). Nothing here allocates per item beyond the
//! batch arrays themselves; label text lives in one shared byte arena.

use std::ops::Range;

use super::grid::{GridGeom, Level};

/// Marker for plain-old-data instance structs that may be viewed as bytes.
///
/// # Safety
/// Implementors must be `#[repr(C)]`, contain no padding bytes, and consist only of
/// integer/float fields (every bit pattern of the bytes is then initialised memory).
pub unsafe trait Pod: Copy + 'static {}

/// Views a slice of POD values as raw bytes (for GPU uploads).
pub fn as_bytes<T: Pod>(v: &[T]) -> &[u8] {
    // SAFETY: `T: Pod` guarantees no padding and only initialised integer/float fields, so
    // every byte in the slice is initialised; u8 has alignment 1 and the length is exact.
    unsafe { std::slice::from_raw_parts(v.as_ptr().cast::<u8>(), std::mem::size_of_val(v)) }
}

/// RGBA8 colour, gamma (sRGB-encoded) space like egui, straight (not premultiplied) alpha.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Rgba(pub [u8; 4]);

impl Rgba {
    pub const TRANSPARENT: Self = Self([0, 0, 0, 0]);
    pub const fn rgb(r: u8, g: u8, b: u8) -> Self {
        Self([r, g, b, 255])
    }
    pub const fn new(r: u8, g: u8, b: u8, a: u8) -> Self {
        Self([r, g, b, a])
    }
}

/// An axis-aligned rectangle in world units (y grows downwards, like the screen).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct WorldRect {
    pub min: [f32; 2],
    pub max: [f32; 2],
}

impl WorldRect {
    pub const EMPTY: Self = Self { min: [f32::INFINITY; 2], max: [f32::NEG_INFINITY; 2] };

    pub const fn new(x0: f32, y0: f32, x1: f32, y1: f32) -> Self {
        Self { min: [x0, y0], max: [x1, y1] }
    }
    pub fn width(&self) -> f32 {
        self.max[0] - self.min[0]
    }
    pub fn height(&self) -> f32 {
        self.max[1] - self.min[1]
    }
    pub fn center(&self) -> [f32; 2] {
        [(self.min[0] + self.max[0]) * 0.5, (self.min[1] + self.max[1]) * 0.5]
    }
    /// True if the rect has positive area (false for NaN edges).
    pub fn is_positive(&self) -> bool {
        self.max[0] > self.min[0] && self.max[1] > self.min[1]
    }
    pub fn contains(&self, p: [f32; 2]) -> bool {
        p[0] >= self.min[0] && p[0] <= self.max[0] && p[1] >= self.min[1] && p[1] <= self.max[1]
    }
    pub fn intersects(&self, o: &Self) -> bool {
        self.min[0] <= o.max[0] && o.min[0] <= self.max[0] && self.min[1] <= o.max[1] && o.min[1] <= self.max[1]
    }
    /// True if `o` lies entirely inside `self`.
    pub fn contains_rect(&self, o: &Self) -> bool {
        o.min[0] >= self.min[0] && o.max[0] <= self.max[0] && o.min[1] >= self.min[1] && o.max[1] <= self.max[1]
    }
    pub fn union(&self, o: &Self) -> Self {
        Self {
            min: [self.min[0].min(o.min[0]), self.min[1].min(o.min[1])],
            max: [self.max[0].max(o.max[0]), self.max[1].max(o.max[1])],
        }
    }
    pub fn expand(&self, dx: f32, dy: f32) -> Self {
        Self { min: [self.min[0] - dx, self.min[1] - dy], max: [self.max[0] + dx, self.max[1] + dy] }
    }
}

/// Rect instance (32 bytes): a rounded rectangle with fill and inner stroke, drawn as one
/// instanced quad with an SDF fragment shader. Radius and stroke width are in points
/// (scaled by the DPI factor in the shader), so they look the same at any zoom.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RectInstance {
    pub min: [f32; 2],
    pub max: [f32; 2],
    pub fill: Rgba,
    pub stroke: Rgba,
    pub radius: f32,
    pub stroke_width: f32,
}

/// Edge instance (40 bytes): a cubic Bezier `p0, c0, c1, p1` in world units, drawn as a
/// ribbon of constant width in points (instanced triangle strip, tessellated in the vertex
/// shader). Straight lines use the line batch, which draws one quad per instance.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EdgeInstance {
    pub p0: [f32; 2],
    pub c0: [f32; 2],
    pub c1: [f32; 2],
    pub p1: [f32; 2],
    pub color: Rgba,
    pub width: f32,
}

/// Glyph instance (32 bytes): one glyph of a label, anchored at the top-left of the
/// labelled rect (plus padding) and clipped to it in the vertex shader. Offsets and sizes
/// are in physical pixels, so text is pixel-exact at every zoom with no CPU work on pan or
/// zoom.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GlyphInstance {
    pub rect_min: [f32; 2],
    pub rect_max: [f32; 2],
    /// Glyph bitmap top-left relative to the label pen origin (baseline-relative y).
    pub offset: [i16; 2],
    pub size: [u8; 2],
    /// Label LOD: the label shows only if the rect is at least this many pixels wide
    /// (inside the padding), i.e. its first few characters fit.
    pub need_px: u16,
    /// Atlas texel origin.
    pub uv: [u16; 2],
    pub color: Rgba,
}

// SAFETY: all four are #[repr(C)] structs of f32/u8/i16/u16 arrays whose sizes add up to
// the struct size (checked by `instance_sizes_have_no_padding`), so they have no padding.
unsafe impl Pod for Rgba {}
// SAFETY: see above.
unsafe impl Pod for RectInstance {}
// SAFETY: see above.
unsafe impl Pod for EdgeInstance {}
// SAFETY: see above.
unsafe impl Pod for GlyphInstance {}

/// Level-of-detail band: `floor(log2(zoom)) + BAND_BIAS`, where zoom is physical pixels per
/// world unit. A band spans a factor of two in zoom.
pub type Band = u8;
pub const BAND_BIAS: i32 = 128;
/// Highest band a camera can be in; `Lod::max == LOD_UNBOUNDED` therefore never excludes.
pub const MAX_BAND: Band = 254;
pub const LOD_UNBOUNDED: Band = 255;

/// Band for a zoom factor (clamped to `0..=MAX_BAND`; non-finite or non-positive zooms map
/// to band 0).
pub fn band_for_zoom(zoom: f32) -> Band {
    if !(zoom.is_finite() && zoom > 0.0) {
        return 0;
    }
    let b = f64::from(zoom).log2().floor() as i32 + BAND_BIAS;
    b.clamp(0, i32::from(MAX_BAND)) as Band
}

/// Smallest zoom of a band (`2^(band - BAND_BIAS)`).
pub fn band_zoom(band: Band) -> f64 {
    (f64::from(band) - f64::from(BAND_BIAS)).exp2()
}

/// Bands in which an item is visible: `min <= band < max`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Lod {
    pub min: Band,
    pub max: Band,
}

impl Lod {
    pub const ALWAYS: Self = Self { min: 0, max: LOD_UNBOUNDED };
    pub const NEVER: Self = Self { min: LOD_UNBOUNDED, max: LOD_UNBOUNDED };

    pub const fn contains(self, band: Band) -> bool {
        self.min <= band && band < self.max
    }

    /// Visible from the first band in which something `size` world units big is at least
    /// `min_px` physical pixels at the band's smallest zoom (so at every zoom in the band).
    pub fn for_size(size: f32, min_px: f32) -> Self {
        if !(size.is_finite() && size > 0.0 && min_px.is_finite() && min_px > 0.0) {
            return if size.is_infinite() && size > 0.0 { Self::ALWAYS } else { Self::NEVER };
        }
        let need = f64::from(min_px) / f64::from(size);
        let mut b = need.log2().ceil() as i32 + BAND_BIAS;
        // log2 may be off by one ulp at exact powers of two: settle it exactly.
        let fits = |b: i32| f64::from(size) * (f64::from(b) - f64::from(BAND_BIAS)).exp2() >= f64::from(min_px);
        while b > 0 && fits(b - 1) {
            b -= 1;
        }
        while b <= i32::from(MAX_BAND) && !fits(b) {
            b += 1;
        }
        if b > i32::from(MAX_BAND) {
            return Self::NEVER;
        }
        Self { min: b.max(0) as Band, max: LOD_UNBOUNDED }
    }
}

/// Bitmap of bands where batch membership changes (some item's `min` or `max`). Moving
/// between two bands needs a rebuild only if a change point lies in between.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ChangePoints([u64; 4]);

impl ChangePoints {
    pub fn add(&mut self, lod: Lod) {
        for b in [lod.min, lod.max] {
            self.0[usize::from(b >> 6)] |= 1 << (b & 63);
        }
    }
    pub fn merge(&mut self, o: &Self) {
        for (a, b) in self.0.iter_mut().zip(o.0) {
            *a |= b;
        }
    }
    fn has(&self, b: Band) -> bool {
        self.0[usize::from(b >> 6)] & (1 << (b & 63)) != 0
    }
    /// True if the visible set can differ between bands `a` and `b`.
    pub fn changes_between(&self, a: Band, b: Band) -> bool {
        let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
        // Membership at band x depends on which change points are <= x.
        (lo + 1..=hi).any(|x| self.has(x))
    }
}

/// One spatially indexed layer of a batch: its instance range and grid.
#[derive(Debug, Clone)]
pub struct Layer {
    pub layer: u8,
    /// Range of sorted indices in the batch.
    pub items: Range<u32>,
    pub grid: GridGeom,
    /// `bin_start[b]..bin_start[b + 1]` are the batch indices in bin `b` (absolute, i.e.
    /// already offset by `items.start`). Length `grid.bins() + 1`.
    pub bin_start: Vec<u32>,
    /// Union of the layer's item bboxes (what the grid covers).
    pub bounds: WorldRect,
}

/// A sorted, spatially indexed set of instances of one kind.
#[derive(Debug, Clone)]
pub struct Batch<T> {
    pub inst: Vec<T>,
    pub lod: Vec<Lod>,
    /// Caller id per sorted index.
    pub id: Vec<u32>,
    /// Layers in ascending draw order.
    pub layers: Vec<Layer>,
    pub change_points: ChangePoints,
}

impl<T> Default for Batch<T> {
    fn default() -> Self {
        Self { inst: Vec::new(), lod: Vec::new(), id: Vec::new(), layers: Vec::new(), change_points: ChangePoints::default() }
    }
}

impl<T> Batch<T> {
    pub fn len(&self) -> usize {
        self.inst.len()
    }
    pub fn is_empty(&self) -> bool {
        self.inst.is_empty()
    }
}

/// Per-item metadata collected by the builder.
#[derive(Debug, Clone, Copy)]
struct Meta {
    id: u32,
    layer: u8,
    lod: Lod,
}

/// A label: byte span in the scene's text arena (`len == 0`: no label).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LabelSpan {
    pub start: u32,
    pub len: u32,
}

/// Style of a rect: fill, inner stroke, corner radius and stroke width in points.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RectStyle {
    pub fill: Rgba,
    pub stroke: Rgba,
    pub radius: f32,
    pub stroke_width: f32,
}

/// Rects smaller than this many physical pixels are left out of a band (auto LOD).
pub const MIN_RECT_PX: f32 = 2.0;

/// Collects items in any order; [`SceneBuilder::build`] sorts and indexes them.
#[derive(Debug, Default)]
pub struct SceneBuilder {
    rects: Vec<RectInstance>,
    rect_meta: Vec<Meta>,
    rect_label: Vec<LabelSpan>,
    lines: Vec<EdgeInstance>,
    line_meta: Vec<Meta>,
    curves: Vec<EdgeInstance>,
    curve_meta: Vec<Meta>,
    text: Vec<u8>,
}

impl SceneBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_capacity(rects: usize, edges: usize) -> Self {
        Self {
            rects: Vec::with_capacity(rects),
            rect_meta: Vec::with_capacity(rects),
            rect_label: Vec::with_capacity(rects),
            curves: Vec::with_capacity(edges),
            curve_meta: Vec::with_capacity(edges),
            ..Self::default()
        }
    }

    /// Adds a rect drawn in `layer` (layers draw in ascending order; items within a layer
    /// should not overlap). Visible once it is at least [`MIN_RECT_PX`] on screen. Returns
    /// its index for [`Self::set_rect_lod`].
    pub fn add_rect(&mut self, id: u32, layer: u8, r: WorldRect, style: RectStyle, label: Option<&[u8]>) -> usize {
        let lod = Lod::for_size(r.width().max(r.height()), MIN_RECT_PX);
        let span = match label {
            Some(t) if !t.is_empty() && self.text.len() + t.len() <= u32::MAX as usize => {
                let start = self.text.len() as u32;
                self.text.extend_from_slice(t);
                LabelSpan { start, len: t.len() as u32 }
            }
            _ => LabelSpan::default(),
        };
        self.rects.push(RectInstance {
            min: r.min,
            max: r.max,
            fill: style.fill,
            stroke: style.stroke,
            radius: style.radius,
            stroke_width: style.stroke_width,
        });
        self.rect_meta.push(Meta { id, layer, lod });
        self.rect_label.push(span);
        self.rects.len() - 1
    }

    /// Overrides the automatic size-based LOD of rect `index`.
    pub fn set_rect_lod(&mut self, index: usize, lod: Lod) {
        self.rect_meta[index].lod = lod;
    }

    /// The LOD of rect `index`.
    pub fn rect_lod(&self, index: usize) -> Lod {
        self.rect_meta[index].lod
    }

    /// Adds a straight edge (one quad per instance).
    pub fn add_line(&mut self, id: u32, p0: [f32; 2], p1: [f32; 2], color: Rgba, width: f32, lod: Lod) {
        self.lines.push(EdgeInstance { p0, c0: p0, c1: p1, p1, color, width });
        self.line_meta.push(Meta { id, layer: 0, lod });
    }

    /// Adds a cubic Bezier edge `p0, c0, c1, p1`.
    pub fn add_curve(&mut self, id: u32, pts: [[f32; 2]; 4], color: Rgba, width: f32, lod: Lod) {
        self.curves.push(EdgeInstance { p0: pts[0], c0: pts[1], c1: pts[2], p1: pts[3], color, width });
        self.curve_meta.push(Meta { id, layer: 0, lod });
    }

    pub fn rect_count(&self) -> usize {
        self.rects.len()
    }

    /// Sorts every batch by (layer, grid bin) and builds the grids.
    pub fn build(self) -> Scene {
        let (rects, perm) = build_batch(self.rects, &self.rect_meta, |r| WorldRect { min: r.min, max: r.max });
        let mut rect_label = Vec::with_capacity(perm.len());
        rect_label.extend(perm.iter().map(|&i| self.rect_label[i as usize]));
        drop(perm);
        let (lines, _) = build_batch(self.lines, &self.line_meta, edge_bbox);
        let (curves, _) = build_batch(self.curves, &self.curve_meta, edge_bbox);
        let mut bounds = WorldRect::EMPTY;
        for l in rects.layers.iter().chain(&lines.layers).chain(&curves.layers) {
            bounds = bounds.union(&l.bounds);
        }
        let max_edge_width = lines.inst.iter().chain(&curves.inst).map(|e| e.width).fold(0.0f32, f32::max);
        Scene { rects, rect_label, lines, curves, text: self.text, bounds, max_edge_width }
    }
}

/// Bounding box of an edge's control polygon (contains the curve).
pub fn edge_bbox(e: &EdgeInstance) -> WorldRect {
    WorldRect {
        min: [e.p0[0].min(e.c0[0]).min(e.c1[0]).min(e.p1[0]), e.p0[1].min(e.c0[1]).min(e.c1[1]).min(e.p1[1])],
        max: [e.p0[0].max(e.c0[0]).max(e.c1[0]).max(e.p1[0]), e.p0[1].max(e.c0[1]).max(e.c1[1]).max(e.p1[1])],
    }
}

/// Sorts `items` by (layer, bin) and builds one grid per layer. Returns the batch and the
/// permutation (`perm[k]` = original index of sorted item `k`).
fn build_batch<T: Copy>(items: Vec<T>, meta: &[Meta], bbox: impl Fn(&T) -> WorldRect) -> (Batch<T>, Vec<u32>) {
    debug_assert_eq!(items.len(), meta.len());
    let n = items.len();
    let mut count = [0u32; 256];
    let mut bounds = [WorldRect::EMPTY; 256];
    let mut change_points = ChangePoints::default();
    for (it, m) in items.iter().zip(meta) {
        count[usize::from(m.layer)] += 1;
        let b = &mut bounds[usize::from(m.layer)];
        *b = b.union(&bbox(it));
        change_points.add(m.lod);
    }
    // Original indices grouped by layer (stable).
    let mut layer_start = [0u32; 257];
    for l in 0..256 {
        layer_start[l + 1] = layer_start[l] + count[l];
    }
    let mut by_layer = vec![0u32; n];
    let mut next = layer_start;
    for (i, m) in meta.iter().enumerate() {
        let s = &mut next[usize::from(m.layer)];
        by_layer[*s as usize] = i as u32;
        *s += 1;
    }
    let mut perm = Vec::with_capacity(n);
    let mut layers = Vec::new();
    for l in 0..256usize {
        if count[l] == 0 {
            continue;
        }
        let idx = &by_layer[layer_start[l] as usize..layer_start[l + 1] as usize];
        let grid = GridGeom::new(bounds[l], idx.len());
        let (p, mut bin_start) = super::grid::sort_by_bin(&grid, idx.iter().map(|&i| bbox(&items[i as usize])), idx.len());
        let base = perm.len() as u32;
        for s in &mut bin_start {
            *s += base;
        }
        perm.extend(p.iter().map(|&k| idx[k as usize]));
        layers.push(Layer { layer: l as u8, items: base..base + idx.len() as u32, grid, bin_start, bounds: bounds[l] });
    }
    drop(by_layer);
    let mut inst = Vec::with_capacity(n);
    let mut lod = Vec::with_capacity(n);
    let mut id = Vec::with_capacity(n);
    for &i in &perm {
        inst.push(items[i as usize]);
        lod.push(meta[i as usize].lod);
        id.push(meta[i as usize].id);
    }
    (Batch { inst, lod, id, layers, change_points }, perm)
}

/// An immutable, indexed scene.
#[derive(Debug, Clone, Default)]
pub struct Scene {
    pub rects: Batch<RectInstance>,
    /// Label per sorted rect.
    pub rect_label: Vec<LabelSpan>,
    pub lines: Batch<EdgeInstance>,
    pub curves: Batch<EdgeInstance>,
    /// Label text arena (UTF-8 expected; invalid bytes render as U+FFFD).
    pub text: Vec<u8>,
    /// Union of all item bboxes (`WorldRect::EMPTY` for an empty scene).
    pub bounds: WorldRect,
    /// Widest edge in points (cull and hit margin).
    pub max_edge_width: f32,
}

impl Scene {
    /// Label bytes of sorted rect `rect` (empty if none).
    pub fn label(&self, rect: usize) -> &[u8] {
        let s = self.rect_label[rect];
        &self.text[s.start as usize..(s.start + s.len) as usize]
    }

    /// Bands where some batch's visible set changes.
    pub fn change_points(&self) -> ChangePoints {
        let mut c = self.rects.change_points;
        c.merge(&self.lines.change_points);
        c.merge(&self.curves.change_points);
        c
    }

    /// Approximate heap bytes held by the scene.
    pub fn heap_bytes(&self) -> usize {
        fn batch<T>(b: &Batch<T>) -> usize {
            let layers: usize = b.layers.iter().map(|l| l.bin_start.capacity() * 4 + l.grid.levels.capacity() * size_of::<Level>()).sum();
            b.inst.capacity() * size_of::<T>() + b.lod.capacity() * size_of::<Lod>() + b.id.capacity() * 4 + layers
        }
        batch(&self.rects) + batch(&self.lines) + batch(&self.curves) + self.rect_label.capacity() * size_of::<LabelSpan>() + self.text.capacity()
    }
}
