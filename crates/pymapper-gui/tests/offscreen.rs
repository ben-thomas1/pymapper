//! Renders the full UI (panels + canvas) offscreen with the real device setup and checks the
//! pixels: the canvas grid lands where egui reports the canvas, and panels cover the rest.
//! Skips (passes with a note) when no GPU adapter is available, e.g. on headless CI.

use egui::{RawInput, Rect, ViewportId, pos2, vec2};
use pymapper_gui::canvas::{CanvasStyle, PhysicalRect};
use pymapper_gui::gpu;
use pymapper_gui::panels::{self, UiState};
use pymapper_gui::render::{FrameInput, Renderer};

const FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;

struct Gpu {
    device: wgpu::Device,
    queue: wgpu::Queue,
}

fn gpu() -> Option<Gpu> {
    let instance = gpu::instance();
    let adapter = match gpu::adapter(&instance, None) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("skipping: {e}");
            return None;
        }
    };
    let (device, queue) = gpu::device(&adapter).expect("device");
    Some(Gpu { device, queue })
}

/// Renders `frames` frames of the UI at `points` size and `ppp`, returns RGBA8 pixels, the
/// pixel size and the canvas rect in pixels.
fn render_ui(g: &Gpu, points: [f32; 2], ppp: f32, frames: usize) -> (Vec<u8>, [u32; 2], PhysicalRect) {
    let size = [(points[0] * ppp) as u32, (points[1] * ppp) as u32];
    let texture = g.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("offscreen"),
        size: wgpu::Extent3d { width: size[0], height: size[1], depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: FORMAT,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    let mut renderer = Renderer::new(&g.device, FORMAT);
    let ctx = egui::Context::default();
    let mut state = UiState::new("proj".to_owned(), "offscreen".to_owned());
    let mut canvas = None;
    for _ in 0..frames {
        let mut raw = RawInput {
            screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(points[0], points[1]))),
            ..RawInput::default()
        };
        raw.viewports.entry(ViewportId::ROOT).or_default().native_pixels_per_point = Some(ppp);
        let mut out = ctx.run_ui(raw, |ui| panels::show(ui, &mut state));
        let primitives = ctx.tessellate(std::mem::take(&mut out.shapes), out.pixels_per_point);
        canvas = PhysicalRect::from_points(state.canvas_rect, out.pixels_per_point, size);
        renderer.render(
            &g.device,
            &g.queue,
            &view,
            FrameInput { primitives: &primitives, textures: &mut out.textures_delta, size, pixels_per_point: ppp, canvas },
        );
        assert!(out.textures_delta.is_empty(), "render must drain texture deltas");
    }

    let bytes_per_row = (size[0] * 4).next_multiple_of(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT);
    let buffer = g.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("readback"),
        size: u64::from(bytes_per_row) * u64::from(size[1]),
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut encoder = g.device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
    encoder.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo {
            texture: &texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        wgpu::TexelCopyBufferInfo {
            buffer: &buffer,
            layout: wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(bytes_per_row), rows_per_image: None },
        },
        wgpu::Extent3d { width: size[0], height: size[1], depth_or_array_layers: 1 },
    );
    g.queue.submit([encoder.finish()]);
    let (tx, rx) = std::sync::mpsc::channel();
    buffer.map_async(wgpu::MapMode::Read, .., move |r| {
        let _ = tx.send(r);
    });
    g.device.poll(wgpu::PollType::wait_indefinitely()).expect("poll");
    rx.recv().expect("map callback").expect("map");
    let mapped = buffer.get_mapped_range(..).expect("mapped range");
    let mut pixels = Vec::with_capacity((size[0] * size[1] * 4) as usize);
    for row in mapped.chunks_exact(bytes_per_row as usize) {
        pixels.extend_from_slice(&row[..(size[0] * 4) as usize]);
    }
    drop(mapped);
    buffer.unmap();
    (pixels, size, canvas.expect("canvas visible"))
}

fn px(pixels: &[u8], size: [u32; 2], x: u32, y: u32) -> [u8; 3] {
    let i = ((y * size[0] + x) * 4) as usize;
    [pixels[i], pixels[i + 1], pixels[i + 2]]
}

fn to_u8(c: [f32; 4]) -> [u8; 3] {
    [(c[0] * 255.0).round() as u8, (c[1] * 255.0).round() as u8, (c[2] * 255.0).round() as u8]
}

fn close(a: [u8; 3], b: [u8; 3]) -> bool {
    a.iter().zip(b).all(|(x, y)| x.abs_diff(y) <= 2)
}

fn check_layout(ppp: f32) {
    let Some(g) = gpu() else { return };
    let (pixels, size, c) = render_ui(&g, [960.0, 640.0], ppp, 3);
    let style = CanvasStyle::DEFAULT;
    let spacing = (style.spacing_points * ppp).round() as u32;
    let bg = to_u8(style.background);

    // Canvas interior, mid-cell: background. Lines at multiples of `spacing` from the origin.
    let y = c.y + spacing + spacing / 2;
    let mid = px(&pixels, size, c.x + spacing + spacing / 2, y);
    assert!(close(mid, bg), "mid-cell {mid:?} vs background {bg:?} (canvas {c:?})");
    let minor = px(&pixels, size, c.x + spacing, y);
    assert!(close(minor, to_u8(style.minor)), "minor line {minor:?} (canvas {c:?})");
    let major = px(&pixels, size, c.x + 5 * spacing, y);
    assert!(close(major, to_u8(style.major)), "major line {major:?} (canvas {c:?})");

    // Panels are opaque egui fill, not canvas background.
    let left = px(&pixels, size, c.x / 2, size[1] / 2);
    let right = px(&pixels, size, (c.x + c.w + size[0]) / 2, size[1] / 2);
    let bottom = px(&pixels, size, c.x + c.w / 2, (c.y + c.h + size[1]) / 2);
    for (name, p) in [("left", left), ("right", right), ("bottom", bottom)] {
        assert!(!close(p, bg), "{name} panel shows canvas background {p:?}");
    }
    // Canvas edges in pixels track the scale factor.
    assert!(c.x as f32 >= panels::LEFT_DEFAULT * ppp - 1.0, "{c:?}");
}

#[test]
fn offscreen_layout_1x() {
    check_layout(1.0);
}

#[test]
fn offscreen_layout_2x() {
    check_layout(2.0);
}
