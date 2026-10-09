//! GPU side of the map: pipelines, static instance buffers, glyph atlas, camera uniform.
//!
//! Instance buffers hold the band-filtered batches and are rewritten only when
//! batch's band generation changes (LOD change); the glyph buffer only when the label
//! set changes. Per frame the renderer writes the 48-byte camera uniform (if it changed)
//! and turns the grid query for the visible rect into a few instance ranges per layer.

use std::borrow::Cow;
use std::num::NonZeroU64;
use std::ops::Range;

use super::band::{BandBatch, QueryScratch};
use super::hit::{CURVE_SEGMENTS, HitKind};
use super::scene::{Batch, EdgeInstance, GlyphInstance, Pod, RectInstance, Rgba, WorldRect, as_bytes};
use super::{LABEL_PAD_POINTS, MapState};
use crate::canvas::PhysicalRect;

/// Draw calls per (batch, layer) are capped at this many ranges (see `grid::limit_ranges`).
pub const MAX_RANGES_PER_LAYER: usize = 16;
const UNIFORM_FLOATS: usize = 12;
const UNIFORM_BYTES: usize = UNIFORM_FLOATS * 4;

/// Instance buffers are sized in multiples of this (bytes).
const BUFFER_GRANULE: u64 = 64 * 1024;
/// Buffers above this size shrink when mostly unused.
const SHRINK_MIN: u64 = 4 * 1024 * 1024;

/// Highlight drawn over the hovered item.
pub const HOVER_COLOR: Rgba = Rgba::rgb(255, 214, 10);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pipe {
    Rect,
    Line,
    Curve,
    Glyph,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Buf {
    Rects,
    Lines,
    Curves,
    Glyphs,
    OverlayRect,
    OverlayEdge,
}

#[derive(Debug, Clone)]
struct DrawCmd {
    pipe: Pipe,
    buf: Buf,
    instances: Range<u32>,
}

/// A growable vertex buffer.
struct GpuBuf {
    buffer: wgpu::Buffer,
    cap: u64,
    label: &'static str,
}

impl GpuBuf {
    fn new(device: &wgpu::Device, label: &'static str) -> Self {
        Self { buffer: create_vertex_buffer(device, label, BUFFER_GRANULE), cap: BUFFER_GRANULE, label }
    }

    /// Makes room for `bytes`. Sized to fit (memory first): re-created when too small, or
    /// when a large buffer would be less than a quarter used (e.g. after zooming out).
    /// Re-creation only happens on LOD changes, never per frame.
    fn ensure(&mut self, device: &wgpu::Device, bytes: u64) {
        let shrink = self.cap > SHRINK_MIN && bytes < self.cap / 4;
        if bytes <= self.cap && !shrink {
            return;
        }
        let cap = bytes.max(256).next_multiple_of(BUFFER_GRANULE);
        self.buffer = create_vertex_buffer(device, self.label, cap);
        self.cap = cap;
    }
}

fn create_vertex_buffer(device: &wgpu::Device, label: &'static str, size: u64) -> wgpu::Buffer {
    device.create_buffer(&wgpu::BufferDescriptor {
        label: Some(label),
        size,
        usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    })
}

/// Writes the band's instances into `buf` (slot order) straight into wgpu's staging
/// memory, one copy per run of consecutive batch items. Returns the bytes written.
fn upload_band<T: Pod>(device: &wgpu::Device, queue: &wgpu::Queue, buf: &mut GpuBuf, batch: &Batch<T>, band: &BandBatch) -> u64 {
    let size = std::mem::size_of::<T>();
    let bytes = (band.len() * size) as u64;
    buf.ensure(device, bytes);
    let Some(nz) = NonZeroU64::new(bytes) else { return 0 };
    let Some(mut view) = queue.write_buffer_with(&buf.buffer, 0, nz) else { return 0 };
    band.for_each_run(|slot, run| {
        let src = as_bytes(&batch.inst[run.start as usize..run.end as usize]);
        let off = slot as usize * size;
        view.slice(off..off + src.len()).copy_from_slice(src);
    });
    bytes
}

fn upload_slice<T: Pod>(device: &wgpu::Device, queue: &wgpu::Queue, buf: &mut GpuBuf, items: &[T]) {
    let bytes = as_bytes(items);
    buf.ensure(device, bytes.len() as u64);
    if !bytes.is_empty() {
        queue.write_buffer(&buf.buffer, 0, bytes);
    }
}

/// GPU memory held by the renderer's buffers and atlas, in bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct GpuMemory {
    pub buffers: u64,
    pub atlas: u64,
}

pub struct MapRenderer {
    rect: wgpu::RenderPipeline,
    line: wgpu::RenderPipeline,
    curve: wgpu::RenderPipeline,
    glyph: wgpu::RenderPipeline,
    layout: wgpu::BindGroupLayout,
    uniform: wgpu::Buffer,
    atlas: wgpu::Texture,
    atlas_size: u32,
    max_atlas: u32,
    bind_group: wgpu::BindGroup,
    rects: GpuBuf,
    lines: GpuBuf,
    curves: GpuBuf,
    glyphs: GpuBuf,
    overlay_rect: GpuBuf,
    overlay_edge: GpuBuf,
    overlay_rects: Vec<RectInstance>,
    overlay_edges: Vec<EdgeInstance>,
    srgb_target: bool,
    last_uniform: [f32; UNIFORM_FLOATS],
    /// (scene generation, band generation) uploaded per batch: rects, lines, curves.
    uploaded_band: [Option<(u64, u64)>; 3],
    uploaded_labels: Option<(u64, u64)>,
    /// Hover and selection marks (generation) last uploaded to the overlay buffers.
    uploaded_hover: Option<(Option<super::hit::Hit>, u64)>,
    glyph_count: u32,
    draws: Vec<DrawCmd>,
    ranges: Vec<Range<u32>>,
    query: QueryScratch,
}

const RECT_ATTRS: [wgpu::VertexAttribute; 6] =
    wgpu::vertex_attr_array![0 => Float32x2, 1 => Float32x2, 2 => Unorm8x4, 3 => Unorm8x4, 4 => Float32, 5 => Float32];
const EDGE_ATTRS: [wgpu::VertexAttribute; 6] =
    wgpu::vertex_attr_array![0 => Float32x2, 1 => Float32x2, 2 => Float32x2, 3 => Float32x2, 4 => Unorm8x4, 5 => Float32];
const GLYPH_ATTRS: [wgpu::VertexAttribute; 7] =
    wgpu::vertex_attr_array![0 => Float32x2, 1 => Float32x2, 2 => Sint16x2, 3 => Uint8x2, 4 => Uint16, 5 => Uint16x2, 6 => Unorm8x4];

fn instance_layout<T>(attrs: &'static [wgpu::VertexAttribute]) -> wgpu::VertexBufferLayout<'static> {
    wgpu::VertexBufferLayout {
        array_stride: std::mem::size_of::<T>() as u64,
        step_mode: wgpu::VertexStepMode::Instance,
        attributes: attrs,
    }
}

fn pipeline(
    device: &wgpu::Device,
    layout: &wgpu::PipelineLayout,
    module: &wgpu::ShaderModule,
    entry: (&str, &str),
    buffer: wgpu::VertexBufferLayout<'static>,
    format: wgpu::TextureFormat,
) -> wgpu::RenderPipeline {
    device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some(entry.0),
        layout: Some(layout),
        vertex: wgpu::VertexState {
            module,
            entry_point: Some(entry.0),
            buffers: &[Some(buffer)],
            compilation_options: wgpu::PipelineCompilationOptions::default(),
        },
        primitive: wgpu::PrimitiveState { topology: wgpu::PrimitiveTopology::TriangleStrip, ..Default::default() },
        depth_stencil: None,
        multisample: wgpu::MultisampleState::default(),
        fragment: Some(wgpu::FragmentState {
            module,
            entry_point: Some(entry.1),
            targets: &[Some(wgpu::ColorTargetState {
                format,
                blend: Some(wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING),
                write_mask: wgpu::ColorWrites::ALL,
            })],
            compilation_options: wgpu::PipelineCompilationOptions::default(),
        }),
        multiview_mask: None,
        cache: None,
    })
}

fn create_atlas(device: &wgpu::Device, size: u32) -> wgpu::Texture {
    device.create_texture(&wgpu::TextureDescriptor {
        label: Some("map_glyph_atlas"),
        size: wgpu::Extent3d { width: size, height: size, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::R8Unorm,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    })
}

fn create_bind_group(device: &wgpu::Device, layout: &wgpu::BindGroupLayout, uniform: &wgpu::Buffer, atlas: &wgpu::Texture) -> wgpu::BindGroup {
    let view = atlas.create_view(&wgpu::TextureViewDescriptor::default());
    device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("map_bind_group"),
        layout,
        entries: &[
            wgpu::BindGroupEntry { binding: 0, resource: uniform.as_entire_binding() },
            wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(&view) },
        ],
    })
}

impl MapRenderer {
    pub fn new(device: &wgpu::Device, format: wgpu::TextureFormat) -> Self {
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("map"),
            source: wgpu::ShaderSource::Wgsl(Cow::Borrowed(include_str!("map.wgsl"))),
        });
        let uniform = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("map_camera"),
            size: UNIFORM_BYTES as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("map_bind_group_layout"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX | wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: NonZeroU64::new(UNIFORM_BYTES as u64),
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: false },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
            ],
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("map_pipeline_layout"),
            bind_group_layouts: &[Some(&layout)],
            immediate_size: 0,
        });
        let pl = &pipeline_layout;
        let rect = pipeline(device, pl, &module, ("vs_rect", "fs_rect"), instance_layout::<RectInstance>(&RECT_ATTRS), format);
        let line = pipeline(device, pl, &module, ("vs_line", "fs_edge"), instance_layout::<EdgeInstance>(&EDGE_ATTRS), format);
        let curve = pipeline(device, pl, &module, ("vs_curve", "fs_edge"), instance_layout::<EdgeInstance>(&EDGE_ATTRS), format);
        let glyph = pipeline(device, pl, &module, ("vs_glyph", "fs_glyph"), instance_layout::<GlyphInstance>(&GLYPH_ATTRS), format);
        let atlas_size = super::ATLAS_SIZE.min(device.limits().max_texture_dimension_2d);
        let atlas = create_atlas(device, atlas_size);
        let bind_group = create_bind_group(device, &layout, &uniform, &atlas);
        Self {
            rect,
            line,
            curve,
            glyph,
            layout,
            uniform,
            atlas,
            atlas_size,
            max_atlas: device.limits().max_texture_dimension_2d.min(8192),
            bind_group,
            rects: GpuBuf::new(device, "map_rects"),
            lines: GpuBuf::new(device, "map_lines"),
            curves: GpuBuf::new(device, "map_curves"),
            glyphs: GpuBuf::new(device, "map_glyphs"),
            overlay_rect: GpuBuf::new(device, "map_overlay_rects"),
            overlay_edge: GpuBuf::new(device, "map_overlay_edges"),
            overlay_rects: Vec::new(),
            overlay_edges: Vec::new(),
            srgb_target: format.is_srgb(),
            last_uniform: [f32::NAN; UNIFORM_FLOATS],
            uploaded_band: [None; 3],
            uploaded_labels: None,
            uploaded_hover: None,
            glyph_count: 0,
            draws: Vec::new(),
            ranges: Vec::new(),
            query: QueryScratch::default(),
        }
    }

    pub fn memory(&self) -> GpuMemory {
        let buffers = [&self.rects, &self.lines, &self.curves, &self.glyphs, &self.overlay_rect, &self.overlay_edge].iter().map(|b| b.cap).sum();
        GpuMemory { buffers, atlas: u64::from(self.atlas_size) * u64::from(self.atlas_size) }
    }

    pub const fn atlas_size(&self) -> u32 {
        self.atlas_size
    }
}

impl MapRenderer {
    /// Per-frame preparation for a canvas at `rect` of a `target`-sized render target:
    /// updates `state` (band/labels, only if needed), uploads what changed, writes the
    /// camera uniform and computes the culled draw list.
    pub fn prepare(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, state: &mut MapState, rect: PhysicalRect, target: [u32; 2], ppp: f32) {
        state.update([rect.w as f32, rect.h as f32], ppp);
        let t = std::time::Instant::now();
        let mut bytes = 0u64;
        let scene = &*state.scene;
        let sg = state.scene_generation;
        let gens = [state.bands.rects.generation, state.bands.lines.generation, state.bands.curves.generation].map(|g| Some((sg, g)));
        if self.uploaded_band[0] != gens[0] {
            bytes += upload_band(device, queue, &mut self.rects, &scene.rects, &state.bands.rects);
        }
        if self.uploaded_band[1] != gens[1] {
            bytes += upload_band(device, queue, &mut self.lines, &scene.lines, &state.bands.lines);
        }
        if self.uploaded_band[2] != gens[2] {
            bytes += upload_band(device, queue, &mut self.curves, &scene.curves, &state.bands.curves);
        }
        let band_changed = self.uploaded_band != gens;
        if band_changed {
            state.stats.band_uploads.record(t.elapsed());
            state.stats.band_upload_bytes = bytes;
        }
        self.uploaded_band = gens;
        self.sync_labels(device, queue, state);
        if self.uploaded_hover != Some((state.hover, state.marks_generation)) || band_changed {
            self.sync_hover(device, queue, state);
        }
        self.write_uniform(queue, state, rect, target);
        self.build_draws(state);
    }

    fn sync_labels(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, state: &mut MapState) {
        if self.uploaded_labels == Some((state.scene_generation, state.labels.generation)) {
            return;
        }
        let full = state.text.as_ref().is_some_and(|t| t.atlas_full);
        if full {
            // Grow if possible; either way start a fresh atlas holding only the glyphs the
            // current labels use (glyphs of earlier label sets are dropped).
            if self.atlas_size < self.max_atlas {
                self.atlas_size = (self.atlas_size * 2).min(self.max_atlas);
                self.atlas = create_atlas(device, self.atlas_size);
                self.bind_group = create_bind_group(device, &self.layout, &self.uniform, &self.atlas);
            }
            if let Some(t) = state.text.as_mut() {
                t.reset_atlas(self.atlas_size, self.atlas_size);
            }
            let px = state.labels.px;
            state.rebuild_labels(px);
        }
        if let Some(t) = state.text.as_mut() {
            for g in &t.pending {
                let n = (g.size[0] * g.size[1]) as usize;
                queue.write_texture(
                    wgpu::TexelCopyTextureInfo {
                        texture: &self.atlas,
                        mip_level: 0,
                        origin: wgpu::Origin3d { x: g.at[0], y: g.at[1], z: 0 },
                        aspect: wgpu::TextureAspect::All,
                    },
                    &t.pending_pixels[g.offset..g.offset + n],
                    wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(g.size[0]), rows_per_image: None },
                    wgpu::Extent3d { width: g.size[0], height: g.size[1], depth_or_array_layers: 1 },
                );
            }
            t.pending.clear();
            t.pending_pixels.clear();
        }
        upload_slice(device, queue, &mut self.glyphs, &state.labels.glyphs);
        self.glyph_count = state.labels.glyphs.len() as u32;
        self.uploaded_labels = Some((state.scene_generation, state.labels.generation));
    }

    fn sync_hover(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, state: &MapState) {
        self.overlay_rects.clear();
        self.overlay_edges.clear();
        self.overlay_rects.extend_from_slice(&state.marks);
        if let Some(h) = state.hover {
            let scene = &*state.scene;
            match h.kind {
                HitKind::Rect => {
                    let r = scene.rects.inst[h.index as usize];
                    self.overlay_rects.push(RectInstance { fill: Rgba::TRANSPARENT, stroke: HOVER_COLOR, stroke_width: 2.0, ..r });
                }
                HitKind::Line | HitKind::Curve => {
                    let b = if h.kind == HitKind::Line { &scene.lines } else { &scene.curves };
                    let e = b.inst[h.index as usize];
                    self.overlay_edges.push(EdgeInstance { color: HOVER_COLOR, width: (e.width + 2.0).max(3.0), ..e });
                }
            }
        }
        upload_slice(device, queue, &mut self.overlay_rect, &self.overlay_rects);
        upload_slice(device, queue, &mut self.overlay_edge, &self.overlay_edges);
        self.uploaded_hover = Some((state.hover, state.marks_generation));
    }

    fn write_uniform(&mut self, queue: &wgpu::Queue, state: &MapState, rect: PhysicalRect, target: [u32; 2]) {
        let c = &state.camera;
        let (line_h, ascent) = state.text.as_ref().map_or((0.0, 0.0), |t| (t.line_height(), t.ascent()));
        let pad = (LABEL_PAD_POINTS * state.pixels_per_point).round();
        let u = [
            c.center[0],
            c.center[1],
            c.zoom,
            if self.srgb_target { 1.0 } else { 0.0 },
            rect.x as f32 + rect.w as f32 * 0.5,
            rect.y as f32 + rect.h as f32 * 0.5,
            target[0].max(1) as f32,
            target[1].max(1) as f32,
            line_h,
            pad,
            ascent,
            state.pixels_per_point,
        ];
        if u == self.last_uniform {
            return;
        }
        let mut bytes = [0u8; UNIFORM_BYTES];
        for (dst, v) in bytes.as_chunks_mut::<4>().0.iter_mut().zip(u) {
            *dst = v.to_ne_bytes();
        }
        queue.write_buffer(&self.uniform, 0, &bytes);
        self.last_uniform = u;
    }

    fn push_batch<T>(&mut self, batch: &Batch<T>, band: &BandBatch, q: &WorldRect, pipe: Pipe, buf: Buf) {
        for li in 0..batch.layers.len() {
            band.query(batch, li, q, MAX_RANGES_PER_LAYER, &mut self.query, &mut self.ranges);
            for r in &self.ranges {
                self.draws.push(DrawCmd { pipe, buf, instances: r.clone() });
            }
        }
    }

    fn build_draws(&mut self, state: &mut MapState) {
        self.draws.clear();
        let scene = &*state.scene;
        let cam = &state.camera;
        let view = cam.visible();
        // Margins: AA fringe for rects; half the widest ribbon for edges.
        let m = 2.0 / cam.zoom;
        self.push_batch(&scene.rects, &state.bands.rects, &view.expand(m, m), Pipe::Rect, Buf::Rects);
        let m = (super::hit::edge_px(scene.max_edge_width, state.pixels_per_point) * 0.5 + 2.0) / cam.zoom;
        let q = view.expand(m, m);
        self.push_batch(&scene.lines, &state.bands.lines, &q, Pipe::Line, Buf::Lines);
        self.push_batch(&scene.curves, &state.bands.curves, &q, Pipe::Curve, Buf::Curves);
        if self.glyph_count > 0 {
            self.draws.push(DrawCmd { pipe: Pipe::Glyph, buf: Buf::Glyphs, instances: 0..self.glyph_count });
        }
        if !self.overlay_rects.is_empty() {
            self.draws.push(DrawCmd { pipe: Pipe::Rect, buf: Buf::OverlayRect, instances: 0..self.overlay_rects.len() as u32 });
        }
        if !self.overlay_edges.is_empty() {
            let pipe = if state.hover.is_some_and(|h| h.kind == HitKind::Line) { Pipe::Line } else { Pipe::Curve };
            self.draws.push(DrawCmd { pipe, buf: Buf::OverlayEdge, instances: 0..self.overlay_edges.len() as u32 });
        }
        state.stats.draw_calls = self.draws.len() as u32;
        state.stats.drawn_instances = self.draws.iter().map(|d| u64::from(d.instances.end - d.instances.start)).sum();
    }

    /// Records the draw calls computed by the last [`Self::prepare`], scissored to `rect`.
    pub fn draw(&self, pass: &mut wgpu::RenderPass<'_>, rect: PhysicalRect) {
        if self.draws.is_empty() {
            return;
        }
        pass.set_scissor_rect(rect.x, rect.y, rect.w, rect.h);
        pass.set_bind_group(0, &self.bind_group, &[]);
        let mut pipe = None;
        let mut buf = None;
        for d in &self.draws {
            if pipe != Some(d.pipe) {
                pass.set_pipeline(match d.pipe {
                    Pipe::Rect => &self.rect,
                    Pipe::Line => &self.line,
                    Pipe::Curve => &self.curve,
                    Pipe::Glyph => &self.glyph,
                });
                pipe = Some(d.pipe);
            }
            if buf != Some(d.buf) {
                let b = match d.buf {
                    Buf::Rects => &self.rects,
                    Buf::Lines => &self.lines,
                    Buf::Curves => &self.curves,
                    Buf::Glyphs => &self.glyphs,
                    Buf::OverlayRect => &self.overlay_rect,
                    Buf::OverlayEdge => &self.overlay_edge,
                };
                pass.set_vertex_buffer(0, b.buffer.slice(..));
                buf = Some(d.buf);
            }
            let verts = match d.pipe {
                Pipe::Curve => 2 * (CURVE_SEGMENTS + 1),
                Pipe::Rect | Pipe::Line | Pipe::Glyph => 4,
            };
            pass.draw(0..verts, d.instances.clone());
        }
    }
}
