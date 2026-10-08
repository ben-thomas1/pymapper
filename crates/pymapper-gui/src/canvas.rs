//! The canvas: the window area between the egui panels, drawn by our own wgpu pipeline
//! before egui so egui never touches it. For now it draws a placeholder grid; the instanced
//! code-map renderer (rects, curves, glyph text, camera in shader) replaces the pipeline but
//! keeps this contract: `prepare` (upload, only when changed) then `draw` (scissored).

use std::borrow::Cow;
use std::num::NonZeroU64;

/// A rectangle in physical (framebuffer) pixels, clamped to the target and never empty.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PhysicalRect {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

impl PhysicalRect {
    /// Converts a rect in egui points to physical pixels for a `target` of
    /// `[width, height]` pixels. Edges (not sizes) are rounded so rects that share an
    /// edge in points share it in pixels at any scale factor. Returns `None` when the
    /// result is empty, an edge is NaN or the scale factor is not a positive finite number.
    pub fn from_points(rect: egui::Rect, pixels_per_point: f32, target: [u32; 2]) -> Option<Self> {
        if !(pixels_per_point.is_finite() && pixels_per_point > 0.0) {
            return None;
        }
        let edge = |v: f32, max: u32| -> Option<u32> {
            let px = (v * pixels_per_point).round();
            if px.is_nan() {
                return None;
            }
            // Float-to-int `as` saturates: negatives and -inf become 0, +inf u32::MAX.
            Some((px as u32).min(max))
        };
        let x0 = edge(rect.min.x, target[0])?;
        let x1 = edge(rect.max.x, target[0])?;
        let y0 = edge(rect.min.y, target[1])?;
        let y1 = edge(rect.max.y, target[1])?;
        (x1 > x0 && y1 > y0).then(|| Self { x: x0, y: y0, w: x1 - x0, h: y1 - y0 })
    }
}

/// Colours are in gamma space (sRGB-encoded), like egui's.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CanvasStyle {
    pub background: [f32; 4],
    pub minor: [f32; 4],
    pub major: [f32; 4],
    /// Minor grid spacing in points (scaled by the DPI factor).
    pub spacing_points: f32,
    /// Grid line width in points.
    pub line_width_points: f32,
}

impl CanvasStyle {
    pub const DEFAULT: Self = Self {
        background: [0.075, 0.078, 0.086, 1.0],
        minor: [0.13, 0.135, 0.15, 1.0],
        major: [0.19, 0.2, 0.22, 1.0],
        spacing_points: 24.0,
        line_width_points: 1.0,
    };
}

const UNIFORM_FLOATS: usize = 20;
const UNIFORM_BYTES: usize = UNIFORM_FLOATS * 4;

pub struct Canvas {
    pipeline: wgpu::RenderPipeline,
    uniform: wgpu::Buffer,
    bind_group: wgpu::BindGroup,
    style: CanvasStyle,
    srgb_target: bool,
    /// Last uploaded uniform contents (starts as NaN so the first prepare uploads).
    last: [f32; UNIFORM_FLOATS],
}

impl Canvas {
    pub fn new(device: &wgpu::Device, format: wgpu::TextureFormat) -> Self {
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("canvas"),
            source: wgpu::ShaderSource::Wgsl(Cow::Borrowed(include_str!("canvas.wgsl"))),
        });
        let uniform = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("canvas_uniform"),
            size: UNIFORM_BYTES as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("canvas_bind_group_layout"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: NonZeroU64::new(UNIFORM_BYTES as u64),
                },
                count: None,
            }],
        });
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("canvas_bind_group"),
            layout: &layout,
            entries: &[wgpu::BindGroupEntry { binding: 0, resource: uniform.as_entire_binding() }],
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("canvas_pipeline_layout"),
            bind_group_layouts: &[Some(&layout)],
            immediate_size: 0,
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("canvas_pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &module,
                entry_point: Some("vs_main"),
                buffers: &[],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            },
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module: &module,
                entry_point: Some("fs_main"),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            }),
            multiview_mask: None,
            cache: None,
        });
        Self {
            pipeline,
            uniform,
            bind_group,
            style: CanvasStyle::DEFAULT,
            srgb_target: format.is_srgb(),
            last: [f32::NAN; UNIFORM_FLOATS],
        }
    }

    /// Clear colour for the frame (the canvas background), encoded for the target format.
    pub fn clear_color(&self) -> wgpu::Color {
        let [r, g, b, a] = encode(self.style.background, self.srgb_target);
        wgpu::Color { r: f64::from(r), g: f64::from(g), b: f64::from(b), a: f64::from(a) }
    }

    /// Uploads the uniform block if it changed since the last frame.
    pub fn prepare(&mut self, queue: &wgpu::Queue, rect: PhysicalRect, pixels_per_point: f32) {
        let u = uniform_block(&self.style, rect, pixels_per_point, self.srgb_target);
        if u == self.last {
            return;
        }
        let mut bytes = [0u8; UNIFORM_BYTES];
        for (dst, v) in bytes.as_chunks_mut::<4>().0.iter_mut().zip(u) {
            *dst = v.to_ne_bytes();
        }
        queue.write_buffer(&self.uniform, 0, &bytes);
        self.last = u;
    }

    /// Draws into `rect` of the current pass. `rect` must lie within the pass's target.
    pub fn draw(&self, pass: &mut wgpu::RenderPass<'_>, rect: PhysicalRect) {
        pass.set_pipeline(&self.pipeline);
        pass.set_bind_group(0, &self.bind_group, &[]);
        pass.set_scissor_rect(rect.x, rect.y, rect.w, rect.h);
        pass.draw(0..3, 0..1);
    }
}

/// Lays out the WGSL `Canvas` uniform struct (see canvas.wgsl).
fn uniform_block(s: &CanvasStyle, r: PhysicalRect, ppp: f32, srgb: bool) -> [f32; UNIFORM_FLOATS] {
    let bg = encode(s.background, srgb);
    let minor = encode(s.minor, srgb);
    let major = encode(s.major, srgb);
    [
        r.x as f32, r.y as f32, r.w as f32, r.h as f32,
        (s.spacing_points * ppp).max(2.0), (s.line_width_points * ppp).max(1.0), 0.0, 0.0,
        bg[0], bg[1], bg[2], bg[3],
        minor[0], minor[1], minor[2], minor[3],
        major[0], major[1], major[2], major[3],
    ]
}

/// Gamma-space colour as the shader must output it: unchanged for a linear (`*Unorm`)
/// target, decoded to linear for an `*UnormSrgb` target (the hardware re-encodes it).
fn encode(c: [f32; 4], srgb_target: bool) -> [f32; 4] {
    if !srgb_target {
        return c;
    }
    let lin = |v: f32| if v <= 0.04045 { v / 12.92 } else { ((v + 0.055) / 1.055).powf(2.4) };
    [lin(c[0]), lin(c[1]), lin(c[2]), c[3]]
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui::{Rect, pos2};

    fn r(x0: f32, y0: f32, x1: f32, y1: f32) -> Rect {
        Rect::from_min_max(pos2(x0, y0), pos2(x1, y1))
    }

    #[test]
    fn scales_by_pixels_per_point() {
        let p = PhysicalRect::from_points(r(10.0, 20.0, 110.0, 70.0), 2.0, [1000, 1000]);
        assert_eq!(p, Some(PhysicalRect { x: 20, y: 40, w: 200, h: 100 }));
    }

    #[test]
    fn fractional_scale_shares_edges() {
        // 1.25x: the left panel's right edge and the canvas's left edge round identically.
        let ppp = 1.25;
        let left = PhysicalRect::from_points(r(0.0, 0.0, 250.3, 100.0), ppp, [4000, 4000]).unwrap();
        let canvas = PhysicalRect::from_points(r(250.3, 0.0, 900.0, 100.0), ppp, [4000, 4000]).unwrap();
        assert_eq!(left.x + left.w, canvas.x);
    }

    #[test]
    fn clamps_to_target() {
        let p = PhysicalRect::from_points(r(-5.0, -5.0, 500.0, 500.0), 1.0, [300, 200]);
        assert_eq!(p, Some(PhysicalRect { x: 0, y: 0, w: 300, h: 200 }));
    }

    #[test]
    fn empty_and_degenerate_are_none() {
        assert_eq!(PhysicalRect::from_points(r(10.0, 10.0, 10.0, 50.0), 1.0, [100, 100]), None);
        assert_eq!(PhysicalRect::from_points(r(10.0, 10.0, 10.2, 50.0), 1.0, [100, 100]), None);
        assert_eq!(PhysicalRect::from_points(r(200.0, 0.0, 300.0, 50.0), 1.0, [100, 100]), None);
        assert_eq!(PhysicalRect::from_points(r(0.0, 0.0, 50.0, 50.0), 1.0, [0, 0]), None);
        assert_eq!(PhysicalRect::from_points(r(0.0, 0.0, f32::NAN, 50.0), 1.0, [100, 100]), None);
        assert_eq!(PhysicalRect::from_points(r(0.0, 0.0, 50.0, 50.0), f32::NAN, [100, 100]), None);
        assert_eq!(PhysicalRect::from_points(r(0.0, 0.0, 50.0, 50.0), 0.0, [100, 100]), None);
        assert_eq!(PhysicalRect::from_points(Rect::NOTHING, 1.0, [100, 100]), None);
        assert_eq!(
            PhysicalRect::from_points(Rect::EVERYTHING, 1.0, [100, 100]),
            Some(PhysicalRect { x: 0, y: 0, w: 100, h: 100 })
        );
    }

    #[test]
    fn uniform_layout_matches_wgsl() {
        let rect = PhysicalRect { x: 1, y: 2, w: 3, h: 4 };
        let u = uniform_block(&CanvasStyle::DEFAULT, rect, 2.0, false);
        assert_eq!(&u[0..4], &[1.0, 2.0, 3.0, 4.0]);
        assert_eq!(u[4], CanvasStyle::DEFAULT.spacing_points * 2.0);
        assert_eq!(u[5], 2.0);
        assert_eq!(&u[8..12], &CanvasStyle::DEFAULT.background);
        assert_eq!(&u[16..20], &CanvasStyle::DEFAULT.major);
        // The WGSL struct is 80 bytes: vec2+vec2+f32+f32+vec2 then 3 x vec4.
        assert_eq!(UNIFORM_BYTES, 80);
    }

    #[test]
    fn srgb_target_decodes_to_linear() {
        assert_eq!(encode([0.5, 0.5, 0.5, 0.7], false), [0.5, 0.5, 0.5, 0.7]);
        let lin = encode([0.0, 0.5, 1.0, 0.7], true);
        assert_eq!(lin[0], 0.0);
        assert!((lin[1] - 0.214).abs() < 1e-3, "{}", lin[1]);
        assert!((lin[2] - 1.0).abs() < 1e-6);
        assert_eq!(lin[3], 0.7, "alpha stays linear");
    }
}
