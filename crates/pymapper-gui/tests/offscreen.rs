//! Renders offscreen with the real device setup and checks the pixels: the full UI (panels
//! and canvas) lands where egui reports the canvas, and the map renderer draws rects, edges
//! and labels where the camera puts them. Skips (passes with a note) when no GPU adapter is
//! available, e.g. on headless CI.

use std::sync::Arc;

use egui::{RawInput, Rect, ViewportId, pos2, vec2};
use pymapper_gui::canvas::{CanvasStyle, PhysicalRect};
use pymapper_gui::gpu;
use pymapper_gui::map::scene::{Lod, RectStyle, Rgba, SceneBuilder, WorldRect};
use pymapper_gui::map::{MapState, stress};
use pymapper_gui::panels::{self, UiState};
use pymapper_gui::render::{FrameInput, Renderer};

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
fn render_ui(
    g: &Gpu,
    format: wgpu::TextureFormat,
    points: [f32; 2],
    ppp: f32,
    frames: usize,
) -> (Vec<u8>, [u32; 2], PhysicalRect) {
    let size = [(points[0] * ppp) as u32, (points[1] * ppp) as u32];
    let texture = offscreen_target(g, format, size);
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    let mut renderer = Renderer::new(&g.device, format);
    let ctx = egui::Context::default();
    let mut state = UiState::new("proj".to_owned(), "offscreen".to_owned());
    let mut map = MapState::default();
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
            FrameInput { primitives: &primitives, textures: &mut out.textures_delta, size, pixels_per_point: ppp, canvas, map: &mut map },
        );
        assert!(out.textures_delta.is_empty(), "render must drain texture deltas");
    }
    (readback(g, &texture, size), size, canvas.expect("canvas visible"))
}

fn offscreen_target(g: &Gpu, format: wgpu::TextureFormat, size: [u32; 2]) -> wgpu::Texture {
    g.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("offscreen"),
        size: wgpu::Extent3d { width: size[0], height: size[1], depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    })
}

/// Copies an RGBA8 texture back to the CPU (tightly packed rows).
fn readback(g: &Gpu, texture: &wgpu::Texture, size: [u32; 2]) -> Vec<u8> {
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
            texture,
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
    pixels
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

/// Checks the rendered layout. Both RGBA8 formats store gamma-space bytes (the sRGB one is
/// encoded by the hardware), so the expected bytes are the style's gamma colours either way.
fn check_layout(format: wgpu::TextureFormat, ppp: f32) {
    let Some(g) = gpu() else { return };
    let (pixels, size, c) = render_ui(&g, format, [960.0, 640.0], ppp, 3);
    let style = CanvasStyle::DEFAULT;
    let bg = to_u8(style.background);

    // Empty map: the canvas interior is background.
    for (fx, fy) in [(0.1, 0.1), (0.5, 0.5), (0.9, 0.8)] {
        let p = px(&pixels, size, c.x + (c.w as f32 * fx) as u32, c.y + (c.h as f32 * fy) as u32);
        assert!(close(p, bg), "canvas {p:?} vs background {bg:?} (canvas {c:?})");
    }

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
    check_layout(wgpu::TextureFormat::Rgba8Unorm, 1.0);
}

#[test]
fn offscreen_layout_2x() {
    check_layout(wgpu::TextureFormat::Rgba8Unorm, 2.0);
}

#[test]
fn offscreen_layout_fractional_scale() {
    check_layout(wgpu::TextureFormat::Rgba8Unorm, 1.5);
}

#[test]
fn offscreen_layout_srgb_target() {
    check_layout(wgpu::TextureFormat::Rgba8UnormSrgb, 2.0);
}

/// Renders only the map (no panels) into the whole target and reads it back.
fn render_map(g: &Gpu, renderer: &mut Renderer, texture: &wgpu::Texture, map: &mut MapState, size: [u32; 2], ppp: f32) -> Vec<u8> {
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    let mut textures = egui::TexturesDelta::default();
    let canvas = Some(PhysicalRect { x: 0, y: 0, w: size[0], h: size[1] });
    renderer.render(&g.device, &g.queue, &view, FrameInput { primitives: &[], textures: &mut textures, size, pixels_per_point: ppp, canvas, map });
    readback(g, texture, size)
}

const FILL_A: Rgba = Rgba::rgb(200, 60, 60);
const FILL_B: Rgba = Rgba::rgb(60, 60, 200);
const GREEN: Rgba = Rgba::rgb(0, 255, 0);
const YELLOW: Rgba = Rgba::rgb(255, 255, 0);

fn test_scene() -> MapState {
    let mut b = SceneBuilder::new();
    let a = RectStyle { fill: FILL_A, stroke: Rgba::rgb(255, 255, 255), radius: 8.0, stroke_width: 2.0 };
    b.add_rect(1, 0, WorldRect::new(100.0, 100.0, 400.0, 400.0), a, Some(b"Alpha_label"));
    let bs = RectStyle { fill: FILL_B, stroke: Rgba::TRANSPARENT, radius: 0.0, stroke_width: 0.0 };
    b.add_rect(2, 1, WorldRect::new(150.0, 250.0, 350.0, 380.0), bs, None);
    b.add_line(3, [500.0, 50.0], [900.0, 50.0], GREEN, 4.0, Lod::ALWAYS);
    b.add_curve(4, [[500.0, 300.0], [600.0, 200.0], [800.0, 400.0], [900.0, 300.0]], YELLOW, 3.0, Lod::ALWAYS);
    MapState::new(Arc::new(b.build()))
}

fn rgb(c: Rgba) -> [u8; 3] {
    [c.0[0], c.0[1], c.0[2]]
}

fn check_map(format: wgpu::TextureFormat) {
    let Some(g) = gpu() else { return };
    let size = [1000, 500];
    let texture = offscreen_target(&g, format, size);
    let mut renderer = Renderer::new(&g.device, format);
    let mut map = test_scene();
    // The first frame fits the scene; then pin the camera so world units are pixels.
    render_map(&g, &mut renderer, &texture, &mut map, size, 1.0);
    map.camera.center = [500.0, 250.0];
    map.camera.zoom = 1.0;
    let p = render_map(&g, &mut renderer, &texture, &mut map, size, 1.0);
    let bg = to_u8(CanvasStyle::DEFAULT.background);
    let at = |x, y| px(&p, size, x, y);
    assert!(close(at(250, 200), rgb(FILL_A)), "fill A {:?}", at(250, 200));
    assert!(close(at(250, 300), rgb(FILL_B)), "child drawn above parent {:?}", at(250, 300));
    assert!(close(at(100, 250), [255, 255, 255]), "stroke {:?}", at(100, 250));
    assert!(close(at(100, 100), bg), "rounded corner {:?}", at(100, 100));
    assert!(close(at(700, 50), rgb(GREEN)), "line {:?}", at(700, 50));
    assert!(close(at(700, 57), bg), "line is 4 px wide {:?}", at(700, 57));
    assert!(close(at(700, 300), rgb(YELLOW)), "curve midpoint {:?}", at(700, 300));
    assert!(close(at(700, 200), bg), "nothing else {:?}", at(700, 200));
    // Label: light text on the dark-ish fill, in the top-left of rect A.
    let bright = (102..260).flat_map(|x| (102..122).map(move |y| (x, y))).filter(|&(x, y)| at(x, y)[1] > 150).count();
    assert!(bright > 30, "label pixels: {bright}");
    assert_eq!(map.stats.labels, 1);

    // Panning moves everything without touching band slots or labels.
    let (band_gen, label_gen) = (map.band_generation, map.labels.generation);
    map.camera.pan_px(50.0, 0.0);
    let p = render_map(&g, &mut renderer, &texture, &mut map, size, 1.0);
    let at = |x, y| px(&p, size, x, y);
    assert!(close(at(430, 200), rgb(FILL_A)), "panned fill {:?}", at(430, 200));
    assert!(close(at(750, 50), rgb(GREEN)));
    assert_eq!((map.band_generation, map.labels.generation), (band_gen, label_gen));

    // Hovering the line highlights it.
    let input = pymapper_gui::map::CanvasInput { hover: Some([750.0, 50.0]), ..pymapper_gui::map::CanvasInput::NONE };
    assert!(map.apply_input(&input, 1.0));
    let p = render_map(&g, &mut renderer, &texture, &mut map, size, 1.0);
    let hover = rgb(pymapper_gui::map::gpu::HOVER_COLOR);
    assert!(close(px(&p, size, 750, 50), hover), "hover {:?}", px(&p, size, 750, 50));

    // Zoomed far out, the small child rect leaves the band and the label stops fitting.
    map.apply_input(&pymapper_gui::map::CanvasInput::NONE, 1.0);
    map.camera.zoom = 0.004;
    let _ = render_map(&g, &mut renderer, &texture, &mut map, size, 1.0);
    assert_eq!(map.bands.rects.len(), 0, "300 world units at 0.004 px/unit is under 2 px");
    assert_eq!(map.stats.labels, 0);
}

#[test]
fn map_renders_rects_edges_labels() {
    check_map(wgpu::TextureFormat::Rgba8Unorm);
}

#[test]
fn map_renders_on_srgb_target() {
    check_map(wgpu::TextureFormat::Rgba8UnormSrgb);
}

#[test]
fn stress_scene_renders() {
    let Some(g) = gpu() else { return };
    let size = [1600, 1000];
    let format = wgpu::TextureFormat::Rgba8Unorm;
    let texture = offscreen_target(&g, format, size);
    let mut renderer = Renderer::new(&g.device, format);
    let mut map = MapState::new(Arc::new(stress::scene(20_000, 20_000, stress::SEED)));
    let bg = to_u8(CanvasStyle::DEFAULT.background);
    for force in [false, true] {
        map.force_all = force;
        let p = render_map(&g, &mut renderer, &texture, &mut map, size, 2.0);
        let covered = p.as_chunks::<4>().0.iter().filter(|c| !close([c[0], c[1], c[2]], bg)).count();
        assert!(covered > (size[0] * size[1] / 2) as usize, "covered {covered}");
        assert!(map.stats.drawn_instances > 0 && map.stats.draw_calls > 0);
    }
    assert_eq!(map.bands.rects.len(), 20_000);
    let mem = renderer.canvas().map().memory();
    assert!(mem.buffers >= 20_000 * 32 + 20_000 * 40, "{mem:?}");
}

#[test]
fn style_sizes_are_points() {
    let Some(g) = gpu() else { return };
    let size = [1000, 500];
    let format = wgpu::TextureFormat::Rgba8Unorm;
    let texture = offscreen_target(&g, format, size);
    let mut renderer = Renderer::new(&g.device, format);
    let mut map = test_scene();
    render_map(&g, &mut renderer, &texture, &mut map, size, 2.0);
    map.camera.center = [500.0, 250.0];
    map.camera.zoom = 1.0;
    let p = render_map(&g, &mut renderer, &texture, &mut map, size, 2.0);
    let bg = to_u8(CanvasStyle::DEFAULT.background);
    // The 4 pt line is 8 px wide at 2x; the 2 pt stroke is 4 px.
    assert!(close(px(&p, size, 700, 53), rgb(GREEN)), "{:?}", px(&p, size, 700, 53));
    assert!(close(px(&p, size, 700, 55), bg), "{:?}", px(&p, size, 700, 55));
    assert!(close(px(&p, size, 103, 250), [255, 255, 255]), "{:?}", px(&p, size, 103, 250));
    assert!(close(px(&p, size, 105, 250), rgb(FILL_A)), "{:?}", px(&p, size, 105, 250));
}

#[test]
fn glyph_atlas_grows_when_full() {
    let Some(g) = gpu() else { return };
    let size = [1600, 600];
    let format = wgpu::TextureFormat::Rgba8Unorm;
    let texture = offscreen_target(&g, format, size);
    let mut renderer = Renderer::new(&g.device, format);
    // Many labels over all printable ASCII at a large size (12x DPI: ~130 px glyphs, 4
    // subpixel bins each) do not fit a 1024^2 atlas.
    let ascii: Vec<u8> = (33u8..127).cycle().take(400).collect();
    let mut b = SceneBuilder::new();
    let style = RectStyle { fill: FILL_A, stroke: Rgba::TRANSPARENT, radius: 0.0, stroke_width: 0.0 };
    for r in 0..6u32 {
        for c in 0..6u32 {
            let (x, y) = (c as f32 * 800.0, r as f32 * 300.0);
            let at = ((r * 6 + c) * 7 % 94) as usize;
            b.add_rect(r * 6 + c, 0, WorldRect::new(x, y, x + 790.0, y + 290.0), style, Some(&ascii[at..at + 12]));
        }
    }
    let mut map = MapState::new(Arc::new(b.build()));
    for _ in 0..4 {
        map.camera.center = [2400.0, 900.0];
        map.camera.zoom = 1.0;
        render_map(&g, &mut renderer, &texture, &mut map, size, 12.0);
    }
    assert!(renderer.canvas().map().atlas_size() > 1024, "{}", renderer.canvas().map().atlas_size());
    assert!(!map.text.as_ref().unwrap().atlas_full);
    assert!(map.stats.glyphs > 0);
}

#[test]
fn label_only_rects_draw_only_their_label() {
    let Some(g) = gpu() else { return };
    let size = [600, 300];
    let format = wgpu::TextureFormat::Rgba8Unorm;
    let texture = offscreen_target(&g, format, size);
    let mut renderer = Renderer::new(&g.device, format);
    let mut b = SceneBuilder::new();
    let fill = RectStyle { fill: FILL_B, stroke: Rgba::TRANSPARENT, radius: 0.0, stroke_width: 0.0 };
    b.add_rect(1, 0, WorldRect::new(50.0, 50.0, 550.0, 250.0), fill, None);
    b.add_rect(1, 0, WorldRect::new(50.0, 50.0, 550.0, 250.0), RectStyle::label_only(FILL_B), Some(b"Carrier_label"));
    let mut map = MapState::new(Arc::new(b.build()));
    render_map(&g, &mut renderer, &texture, &mut map, size, 1.0);
    map.camera.center = [300.0, 150.0];
    map.camera.zoom = 1.0;
    let p = render_map(&g, &mut renderer, &texture, &mut map, size, 1.0);
    let at = |x, y| px(&p, size, x, y);
    assert!(close(at(300, 200), rgb(FILL_B)), "the carrier hides nothing {:?}", at(300, 200));
    let bright = (52..250).flat_map(|x| (52..72).map(move |y| (x, y))).filter(|&(x, y)| at(x, y)[1] > 150).count();
    assert!(bright > 30, "label pixels: {bright}");
    assert_eq!(map.stats.labels, 1);
}

#[test]
fn code_map_renders_and_reveals_children_on_zoom() {
    use pymapper_gui::codemap::load::{LoadParams, load};
    use pymapper_gui::codemap::scene::SceneParams;
    use pymapper_gui::codemap::{CodeMap, MapParams};
    let Some(g) = gpu() else { return };
    let corpus = std::fs::canonicalize(concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/corpus/sample_python")).expect("corpus");
    let lp = LoadParams { path: corpus, isolated: true, deps: Some(false), stdlib: Some(false), cache_dir: None, no_cache: true };
    let cm = CodeMap::build(Arc::new(load(&lp, &|_| {}).expect("load")), MapParams::default());
    let (scene, legend, _) = cm.scene(&SceneParams::default());
    assert!(legend.edges > 0);
    let size = [1200, 800];
    let format = wgpu::TextureFormat::Rgba8Unorm;
    let texture = offscreen_target(&g, format, size);
    let mut renderer = Renderer::new(&g.device, format);
    let mut map = MapState::new(Arc::new(scene));
    let p = render_map(&g, &mut renderer, &texture, &mut map, size, 2.0);
    let bg = to_u8(CanvasStyle::DEFAULT.background);
    let covered = p.as_chunks::<4>().0.iter().filter(|c| !close([c[0], c[1], c[2]], bg)).count();
    assert!(covered > (size[0] * size[1] / 2) as usize, "covered {covered}");
    let (fit_rects, fit_curves) = (map.bands.rects.len(), map.bands.curves.len());
    assert!(fit_rects > 0);
    // Zooming far in shows deeper cells (children revealed) and switches edge levels.
    let max_band = cm.reveal.change_bands().last().copied().expect("bands");
    let zoom = pymapper_gui::map::scene::band_zoom(max_band) as f32 * 1.5;
    map.camera.zoom = zoom.min(map.camera.max_zoom);
    let rebuilds = map.stats.band_rebuilds.count;
    render_map(&g, &mut renderer, &texture, &mut map, size, 2.0);
    assert!(map.stats.band_rebuilds.count > rebuilds);
    assert!(map.bands.rects.len() > fit_rects, "{} -> {}", fit_rects, map.bands.rects.len());
    assert_ne!(map.bands.curves.len(), fit_curves, "edges switch rollup level");
}

#[test]
fn overlays_draw_over_the_scene_in_their_bands() {
    use pymapper_gui::map::{OVERLAY_HIGHLIGHT, OVERLAY_MARKS};
    let Some(g) = gpu() else { return };
    let format = wgpu::TextureFormat::Rgba8Unorm;
    let size = [1000, 500];
    let texture = offscreen_target(&g, format, size);
    let mut renderer = Renderer::new(&g.device, format);
    let mut map = test_scene();
    render_map(&g, &mut renderer, &texture, &mut map, size, 1.0);
    map.camera.center = [500.0, 250.0];
    map.camera.zoom = 1.0;
    // Highlight: a translucent veil over everything plus a green outline on rect A.
    let mut b = SceneBuilder::new();
    let veil = b.add_rect(0, 0, WorldRect::new(0.0, 0.0, 1000.0, 500.0), RectStyle { fill: Rgba::new(0, 0, 0, 128), stroke: Rgba::TRANSPARENT, radius: 0.0, stroke_width: 0.0 }, None);
    b.set_rect_lod(veil, Lod::ALWAYS);
    let o = b.add_rect(1, 1, WorldRect::new(100.0, 100.0, 400.0, 400.0), RectStyle { fill: Rgba::TRANSPARENT, stroke: GREEN, radius: 8.0, stroke_width: 4.0 }, None);
    b.set_rect_lod(o, Lod::ALWAYS);
    map.set_overlay(OVERLAY_HIGHLIGHT, Arc::new(b.build()));
    // Marks: a yellow dot that only shows from band 129 (zoom >= 2).
    let mut m = SceneBuilder::new();
    let d = m.add_rect(2, 0, WorldRect::new(240.0, 190.0, 260.0, 210.0), RectStyle { fill: YELLOW, stroke: Rgba::TRANSPARENT, radius: 0.0, stroke_width: 0.0 }, None);
    m.set_rect_lod(d, Lod { min: 129, max: 255 });
    map.set_overlay(OVERLAY_MARKS, Arc::new(m.build()));
    let p = render_map(&g, &mut renderer, &texture, &mut map, size, 1.0);
    let at = |x, y| px(&p, size, x, y);
    let dim = |c: Rgba| [c.0[0] / 2, c.0[1] / 2, c.0[2] / 2];
    assert!(close(at(250, 200), dim(FILL_A)), "veiled fill {:?}", at(250, 200));
    assert!(close(at(101, 250), rgb(GREEN)), "outline over the veil {:?}", at(101, 250));
    assert!(!close(at(250, 200), rgb(YELLOW)), "mark hidden below its band");
    // Zoom 2x about the dot: the mark's band shows it (under the highlight's veil, which is
    // the top overlay).
    map.camera.center = [250.0, 200.0];
    map.camera.zoom = 2.0;
    let p = render_map(&g, &mut renderer, &texture, &mut map, size, 1.0);
    assert!(close(px(&p, size, 500, 250), dim(YELLOW)), "mark in its band {:?}", px(&p, size, 500, 250));
    map.set_overlay(OVERLAY_HIGHLIGHT, Arc::new(SceneBuilder::new().build()));
    let p = render_map(&g, &mut renderer, &texture, &mut map, size, 1.0);
    assert!(close(px(&p, size, 500, 250), rgb(YELLOW)), "mark without the veil {:?}", px(&p, size, 500, 250));
    // Replacing the scene clears the overlays.
    map.set_scene(Arc::new(SceneBuilder::new().build()));
    assert!(map.overlays.iter().all(|o| o.scene.rects.is_empty()));
}

/// Selection marks draw in the overlay pass, and swapping scenes (view tabs) re-uploads
/// everything and restores the saved camera.
#[test]
fn selection_marks_and_scene_swaps_render() {
    use pymapper_gui::map::scene::RectInstance;
    use pymapper_gui::views::scene::SELECT_COLOR;
    let Some(g) = gpu() else { return };
    let size = [1000, 500];
    let format = wgpu::TextureFormat::Rgba8Unorm;
    let texture = offscreen_target(&g, format, size);
    let mut renderer = Renderer::new(&g.device, format);
    let mut map = test_scene();
    render_map(&g, &mut renderer, &texture, &mut map, size, 1.0);
    map.camera.center = [500.0, 250.0];
    map.camera.zoom = 1.0;
    let bg = to_u8(CanvasStyle::DEFAULT.background);
    // An outline in empty space.
    let mark = RectInstance { min: [550.0, 100.0], max: [850.0, 200.0], fill: Rgba::TRANSPARENT, stroke: SELECT_COLOR, radius: 0.0, stroke_width: 3.0 };
    map.set_marks(&[mark]);
    let p = render_map(&g, &mut renderer, &texture, &mut map, size, 1.0);
    assert!(close(px(&p, size, 700, 101), rgb(SELECT_COLOR)), "mark stroke {:?}", px(&p, size, 700, 101));
    assert!(close(px(&p, size, 700, 150), bg), "mark is hollow");
    map.set_marks(&[]);
    let p = render_map(&g, &mut renderer, &texture, &mut map, size, 1.0);
    assert!(close(px(&p, size, 700, 101), bg), "mark cleared");
    // Swap to another scene (fitted), then back with the saved camera.
    let saved = map.fitted_camera().expect("fitted");
    let first = Arc::clone(&map.scene);
    let mut b = SceneBuilder::new();
    b.add_rect(9, 0, WorldRect::new(0.0, 0.0, 50.0, 50.0), RectStyle { fill: GREEN, stroke: GREEN, radius: 0.0, stroke_width: 0.0 }, None);
    map.show_scene(Arc::new(b.build()), None);
    let p = render_map(&g, &mut renderer, &texture, &mut map, size, 1.0);
    assert!(close(px(&p, size, 500, 250), rgb(GREEN)), "other scene fitted {:?}", px(&p, size, 500, 250));
    map.show_scene(first, Some(saved));
    let p = render_map(&g, &mut renderer, &texture, &mut map, size, 1.0);
    assert_eq!((map.camera.center, map.camera.zoom), (saved.center, saved.zoom));
    assert!(close(px(&p, size, 250, 200), rgb(FILL_A)), "first scene again {:?}", px(&p, size, 250, 200));
    assert!(close(px(&p, size, 700, 50), rgb(GREEN)), "its line re-uploaded {:?}", px(&p, size, 700, 50));
}
