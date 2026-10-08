//! Offscreen canvas benchmark: renders the synthetic stress scene into a 2880x1800 texture
//! (no window, no vsync) and reports per-frame times for several camera scenarios, LOD
//! rebuild costs, hit-test times and memory.
//!
//! ```text
//! cargo run --release -p pymapper-gui --example canvas_bench -- [RECTS] [EDGES] [--frames N] [--dump DIR]
//! ```
//!
//! Frame time = `Renderer::render` (map update, uploads, encode, submit) plus waiting for
//! the GPU to finish (`device.poll(wait)`), so it is the serialised CPU + GPU cost of one
//! frame. `cpu` is the `render` call alone.

use std::io::Write as _;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, bail};
use pymapper_gui::canvas::PhysicalRect;
use pymapper_gui::gpu;
use pymapper_gui::map::stress::{self, Rng};
use pymapper_gui::map::{MapState, MapStats};
use pymapper_gui::render::{FrameInput, Renderer};
use pymapper_gui::stats::{Samples, process_usage};

const SIZE: [u32; 2] = [2880, 1800];
const PPP: f32 = 2.0;
const FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Bgra8Unorm;

struct Bench {
    device: wgpu::Device,
    queue: wgpu::Queue,
    texture: wgpu::Texture,
    renderer: Renderer,
    dump: Option<PathBuf>,
}

impl Bench {
    /// Renders one frame and waits for the GPU. Returns (cpu, total).
    fn frame(&mut self, map: &mut MapState) -> Result<(Duration, Duration)> {
        let view = self.texture.create_view(&wgpu::TextureViewDescriptor::default());
        let mut textures = egui::TexturesDelta::default();
        let canvas = Some(PhysicalRect { x: 0, y: 0, w: SIZE[0], h: SIZE[1] });
        let t0 = Instant::now();
        let input = FrameInput { primitives: &[], textures: &mut textures, size: SIZE, pixels_per_point: PPP, canvas, map };
        self.renderer.render(&self.device, &self.queue, &view, input);
        let t1 = Instant::now();
        self.device.poll(wgpu::PollType::wait_indefinitely()).context("poll")?;
        Ok((t1 - t0, t1.elapsed() + (t1 - t0)))
    }

    /// Writes the current target as a binary PPM (convert with `sips -s format png`).
    fn dump(&self, name: &str) -> Result<()> {
        let Some(dir) = &self.dump else { return Ok(()) };
        let bpr = (SIZE[0] * 4).next_multiple_of(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT);
        let buffer = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("dump"),
            size: u64::from(bpr) * u64::from(SIZE[1]),
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut enc = self.device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
        enc.copy_texture_to_buffer(
            self.texture.as_image_copy(),
            wgpu::TexelCopyBufferInfo { buffer: &buffer, layout: wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(bpr), rows_per_image: None } },
            wgpu::Extent3d { width: SIZE[0], height: SIZE[1], depth_or_array_layers: 1 },
        );
        self.queue.submit([enc.finish()]);
        buffer.map_async(wgpu::MapMode::Read, .., |_| {});
        self.device.poll(wgpu::PollType::wait_indefinitely()).context("poll")?;
        let data = buffer.get_mapped_range(..).context("map")?;
        let mut out = Vec::with_capacity((SIZE[0] * SIZE[1] * 3) as usize + 32);
        write!(out, "P6\n{} {}\n255\n", SIZE[0], SIZE[1])?;
        for row in data.chunks_exact(bpr as usize) {
            for p in row[..(SIZE[0] * 4) as usize].as_chunks::<4>().0 {
                out.extend_from_slice(&[p[2], p[1], p[0]]); // BGRA -> RGB
            }
        }
        std::fs::write(dir.join(format!("{name}.ppm")), out)?;
        Ok(())
    }
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1e3
}

fn summary(label: &str, cpu: &Samples, total: &Samples, stats: &MapStats) {
    let mut scratch = Vec::new();
    let c = cpu.summary(&mut scratch);
    let t = total.summary(&mut scratch);
    println!(
        "  {label:<34} frame p50 {:>7.3} p95 {:>7.3} max {:>7.3} ms | cpu p50 {:>6.3} max {:>6.3} ms | {:>8} inst {:>4} draws",
        ms(t.p50),
        ms(t.p95),
        ms(t.max),
        ms(c.p50),
        ms(c.max),
        stats.drawn_instances,
        stats.draw_calls
    );
}

fn main() -> Result<()> {
    let mut nums = Vec::new();
    let mut frames = 240u32;
    let mut dump = None;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--frames" => frames = args.next().context("--frames N")?.parse()?,
            "--dump" => dump = Some(PathBuf::from(args.next().context("--dump DIR")?)),
            _ => nums.push(a.parse::<usize>().with_context(|| format!("bad argument {a}"))?),
        }
    }
    let rects = nums.first().copied().unwrap_or(1_000_000);
    let edges = nums.get(1).copied().unwrap_or(rects);
    if frames == 0 {
        bail!("--frames must be positive");
    }

    let instance = gpu::instance();
    let adapter = gpu::adapter(&instance, None)?;
    let (device, queue) = gpu::device(&adapter)?;
    println!("canvas_bench: {}", gpu::describe(&adapter).replace('\n', "; "));
    println!("target {}x{} px @ {PPP}x, {frames} frames per scenario", SIZE[0], SIZE[1]);
    let rss0 = process_usage().peak_rss_bytes;

    let t = Instant::now();
    let scene = stress::scene(rects, edges, stress::SEED);
    let build = t.elapsed();
    let bounds = scene.bounds;
    let scene_bytes = scene.heap_bytes();
    println!(
        "scene: {} rects, {} curves; build {:.1} ms; scene heap {:.1} MiB; peak RSS after build {:.1} MiB (start {:.1})",
        scene.rects.len(),
        scene.curves.len(),
        ms(build),
        scene_bytes as f64 / 1048576.0,
        process_usage().peak_rss_bytes as f64 / 1048576.0,
        rss0 as f64 / 1048576.0
    );

    let texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("bench_target"),
        size: wgpu::Extent3d { width: SIZE[0], height: SIZE[1], depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: FORMAT,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let renderer = Renderer::new(&device, FORMAT);
    let mut b = Bench { device, queue, texture, renderer, dump };
    let mut map = MapState::new(Arc::new(scene));

    // First frame: fit, band build, label layout, uploads (and pipeline warm-up).
    let (_, first) = b.frame(&mut map)?;
    println!("first frame (fit + band build + labels + upload): {:.1} ms", ms(first));
    let fit_center = map.camera.center;
    let fit_zoom = map.camera.zoom;
    b.dump("fit")?;

    let mut cpu = Samples::with_capacity(frames as usize);
    let mut total = Samples::with_capacity(frames as usize);
    let mut run = |b: &mut Bench, map: &mut MapState, label: &str, cam: &dyn Fn(f32, &mut MapState)| -> Result<()> {
        cpu.clear();
        total.clear();
        let before = map.stats;
        // Warm-up frame (not recorded) so a scenario's first band build is not counted.
        cam(0.0, map);
        b.frame(map)?;
        for i in 0..frames {
            cam(i as f32 / frames as f32, map);
            let (c, t) = b.frame(map)?;
            cpu.push(c);
            total.push(t);
        }
        summary(label, &cpu, &total, &map.stats);
        let br = map.stats.band_rebuilds.count - before.band_rebuilds.count;
        let lr = map.stats.label_rebuilds.count - before.label_rebuilds.count;
        if br + lr > 0 {
            println!("  {:<34} band rebuilds {br}, label rebuilds {lr}", "");
        }
        b.dump(&label.replace(|c: char| !c.is_ascii_alphanumeric(), "_"))?;
        Ok(())
    };

    println!("scenarios:");
    run(&mut b, &mut map, "static fit view (LOD)", &|_, m| {
        m.camera.center = fit_center;
        m.camera.zoom = fit_zoom;
    })?;
    run(&mut b, &mut map, "pan at fit zoom", &|t, m| {
        let a = std::f32::consts::TAU * t;
        m.camera.center = [fit_center[0] + 800.0 * a.sin(), fit_center[1] + 500.0 * a.cos()];
        m.camera.zoom = fit_zoom;
    })?;
    run(&mut b, &mut map, "pan zoomed 16x (culling)", &|t, m| {
        m.camera.center = [bounds.min[0] + bounds.width() * (0.2 + 0.6 * t), bounds.center()[1]];
        m.camera.zoom = fit_zoom * 16.0;
    })?;
    run(&mut b, &mut map, "pan zoomed 128x (culling)", &|t, m| {
        m.camera.center = [bounds.min[0] + bounds.width() * (0.3 + 0.4 * t), bounds.center()[1]];
        m.camera.zoom = fit_zoom * 128.0;
    })?;
    run(&mut b, &mut map, "zoom sweep 1x..128x..1x", &|t, m| {
        let (c, z) = stress::camera_path(t, &bounds, fit_zoom / 0.95, 7.0);
        m.camera.center = c;
        m.camera.zoom = z.clamp(m.camera.min_zoom, m.camera.max_zoom);
    })?;
    let rebuild = map.stats.band_rebuilds;
    let labels = map.stats.label_rebuilds;
    println!(
        "  LOD band rebuilds so far: {} (mean {:.2} ms, max {:.2} ms); label rebuilds {} (mean {:.2} ms, max {:.2} ms)",
        rebuild.count,
        ms(rebuild.mean()),
        ms(rebuild.max),
        labels.count,
        ms(labels.mean()),
        ms(labels.max)
    );
    map.force_all = true;
    run(&mut b, &mut map, "everything drawn, fit (worst case)", &|t, m| {
        let a = std::f32::consts::TAU * t;
        m.camera.center = [fit_center[0] + 400.0 * a.sin(), fit_center[1]];
        m.camera.zoom = fit_zoom;
    })?;
    map.force_all = false;
    let mem = b.renderer.canvas().map().memory();

    // Hit testing (hover) at three zoom levels.
    let mut rng = Rng::new(1);
    for (label, mul) in [("fit", 1.0f32), ("16x", 16.0), ("128x", 128.0)] {
        map.camera.center = [bounds.center()[0] + bounds.width() * 0.1, bounds.center()[1]];
        map.camera.zoom = fit_zoom * mul;
        b.frame(&mut map)?; // rebuild the band for this zoom
        let mut hits = 0u32;
        let n = 20_000;
        let t = Instant::now();
        let mut worst = Duration::ZERO;
        for _ in 0..n {
            let s = [rng.f32() * SIZE[0] as f32, rng.f32() * SIZE[1] as f32];
            let t1 = Instant::now();
            hits += u32::from(map.hit_test(s, 3.0 * PPP).is_some());
            worst = worst.max(t1.elapsed());
        }
        let mean = t.elapsed() / n;
        println!("hit test at {label:>4}: mean {:.2} us, max {:.1} us ({hits}/{n} hits)", mean.as_secs_f64() * 1e6, worst.as_secs_f64() * 1e6);
    }
    println!(
        "memory: scene {:.1} MiB CPU, GPU buffers {:.1} MiB, atlas {:.1} MiB, peak RSS {:.1} MiB",
        scene_bytes as f64 / 1048576.0,
        mem.buffers as f64 / 1048576.0,
        mem.atlas as f64 / 1048576.0,
        process_usage().peak_rss_bytes as f64 / 1048576.0
    );
    Ok(())
}
