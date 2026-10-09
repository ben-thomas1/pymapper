//! Code map benchmark on a real project: loads it in-process through core, builds the code
//! map (layout, reveal bands, edge table) and its scenes, then renders offscreen (2880x1800
//! at 2x, no window, no vsync) and reports per-frame times for camera scenarios that cross
//! the semantic-zoom bands.
//!
//! ```text
//! cargo run --release -p pymapper-gui --example codemap_bench -- PATH [--[no-]deps] [--[no-]stdlib]
//!     [--isolated] [--cache-dir DIR] [--no-cache] [--frames N] [--dump DIR]
//! ```
//!
//! Frame time = `Renderer::render` (map update, uploads, encode, submit) plus waiting for the
//! GPU (`device.poll(wait)`): the serialised CPU + GPU cost of one frame.

use std::io::Write as _;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, bail};
use pymapper_core::layout::SizeMetric;
use pymapper_gui::canvas::PhysicalRect;
use pymapper_gui::codemap::load::{LoadParams, load};
use pymapper_gui::codemap::metrics::{ColorMetric, EdgeMetric};
use pymapper_gui::codemap::scene::SceneParams;
use pymapper_gui::codemap::{CodeMap, MapParams};
use pymapper_gui::gpu;
use pymapper_gui::map::stress::Rng;
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
                out.extend_from_slice(&[p[2], p[1], p[0]]);
            }
        }
        std::fs::create_dir_all(dir)?;
        std::fs::write(dir.join(format!("{name}.ppm")), out)?;
        Ok(())
    }
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1e3
}

fn mib(b: usize) -> f64 {
    b as f64 / 1048576.0
}

fn summary(label: &str, cpu: &Samples, total: &Samples, stats: &MapStats) {
    let mut scratch = Vec::new();
    let c = cpu.summary(&mut scratch);
    let t = total.summary(&mut scratch);
    println!(
        "  {label:<30} frame p50 {:>7.3} p95 {:>7.3} max {:>7.3} ms | cpu p50 {:>6.3} max {:>6.3} ms | {:>7} inst {:>3} draws",
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
    let mut lp = LoadParams::default();
    let mut path = None;
    let mut frames = 240u32;
    let mut dump = None;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--deps" | "--no-deps" => lp.deps = Some(a == "--deps"),
            "--stdlib" | "--no-stdlib" => lp.stdlib = Some(a == "--stdlib"),
            "--isolated" => lp.isolated = true,
            "--no-cache" => lp.no_cache = true,
            "--cache-dir" => lp.cache_dir = Some(PathBuf::from(args.next().context("--cache-dir DIR")?)),
            "--frames" => frames = args.next().context("--frames N")?.parse()?,
            "--dump" => dump = Some(PathBuf::from(args.next().context("--dump DIR")?)),
            _ if a.starts_with('-') => bail!("unknown option {a}"),
            _ => path = Some(PathBuf::from(a)),
        }
    }
    if frames == 0 {
        bail!("--frames must be positive");
    }
    lp.path = std::fs::canonicalize(path.unwrap_or_else(|| PathBuf::from(".")))?;

    let t = Instant::now();
    let analysis = Arc::new(load(&lp, &|_| {})?);
    let load_time = t.elapsed();
    let ix = &analysis.index;
    println!("codemap_bench: {}", lp.path.display());
    println!(
        "load: {:.0} ms ({} files, {} nodes, {} call pairs, {} import edges); peak RSS {:.1} MiB",
        ms(load_time),
        ix.file_count(),
        ix.node_count(),
        analysis.rollup.stats().call_pairs,
        analysis.rollup.stats().import_edges,
        mib(process_usage().peak_rss_bytes as usize)
    );
    for (name, d) in &analysis.phases {
        println!("  {name:<22} {:>8.1} ms", ms(*d));
    }

    let t = Instant::now();
    let map = CodeMap::build(Arc::clone(&analysis), MapParams::default());
    println!(
        "code map build: {:.1} ms ({} levels, {} from cache; {} edges, {} bands); heap {:.1} MiB; peak RSS {:.1} MiB",
        ms(t.elapsed()),
        map.layout.computed(),
        map.layout_from_cache,
        map.edges.len(),
        map.reveal.change_bands().len(),
        mib(map.heap_bytes()),
        mib(process_usage().peak_rss_bytes as usize)
    );
    for (name, d) in &map.phases {
        println!("  {name:<22} {:>8.1} ms", ms(*d));
    }
    // A size-metric change (cc sum) rebuilds layout, reveal bands and edges. It also
    // replaces the one-file layout cache, so the next run lays out from scratch.
    let t = Instant::now();
    let m = CodeMap::build(Arc::clone(&analysis), MapParams { size: SizeMetric::CcSum, ..MapParams::default() });
    println!("  rebuild for size CcSum: {:.1} ms ({} edges)", ms(t.elapsed()), m.edges.len());
    drop(m);

    println!("scene builds:");
    let mut scene = None;
    for edge in EdgeMetric::ALL {
        let p = SceneParams { edge, ..SceneParams::default() };
        let t = Instant::now();
        let (s, legend, _) = map.scene(&p);
        println!(
            "  edge {:<20} {:>7.1} ms: {} rects, {} edges ({} segments, max weight {}), scene heap {:.1} MiB",
            edge.label(),
            ms(t.elapsed()),
            legend.rects,
            legend.edges,
            legend.segments,
            legend.edge_max,
            mib(s.heap_bytes())
        );
        if edge == EdgeMetric::Sites {
            scene = Some(s);
        }
    }
    for color in [ColorMetric::CcMax, ColorMetric::Loc] {
        let t = Instant::now();
        let _ = map.scene(&SceneParams { color, ..SceneParams::default() });
        println!("  colour {:<18} {:>7.1} ms", color.label(), ms(t.elapsed()));
    }
    let scene = scene.context("no scene")?;
    let bounds = scene.bounds;
    let bands = map.reveal.change_bands();
    println!("semantic zoom change bands: {bands:?}");

    let instance = gpu::instance();
    let adapter = gpu::adapter(&instance, None)?;
    let (device, queue) = gpu::device(&adapter)?;
    println!("gpu: {}", gpu::describe(&adapter).replace('\n', "; "));
    println!("target {}x{} px @ {PPP}x, {frames} frames per scenario", SIZE[0], SIZE[1]);
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
    let mut ms_ = MapState::new(Arc::new(scene));
    let (_, first) = b.frame(&mut ms_)?;
    println!("first frame (fit + band build + labels + upload): {:.1} ms", ms(first));
    let fit_center = ms_.camera.center;
    let fit_zoom = ms_.camera.zoom;
    b.dump("fit")?;
    // A point inside the main region (first party) to zoom into: the centre of the largest
    // first-party module.
    let target = (0..ix.node_count())
        .filter(|&v| !map.layout.muted(v) && ix.kind(v) == pymapper_core::index::NodeKind::Module)
        .max_by_key(|&v| analysis.agg.loc()[v as usize])
        .and_then(|v| map.layout.rect(v))
        .map_or(fit_center, |r| [(r.x0 + r.x1) * 0.5, (r.y0 + r.y1) * 0.5]);

    let mut cpu = Samples::with_capacity(frames as usize);
    let mut total = Samples::with_capacity(frames as usize);
    let mut run = |b: &mut Bench, m: &mut MapState, label: &str, cam: &dyn Fn(f32, &mut MapState)| -> Result<()> {
        cpu.clear();
        total.clear();
        let before = m.stats;
        cam(0.0, m);
        b.frame(m)?;
        for i in 0..frames {
            cam(i as f32 / frames as f32, m);
            let (c, t) = b.frame(m)?;
            cpu.push(c);
            total.push(t);
        }
        summary(label, &cpu, &total, &m.stats);
        let br = m.stats.band_rebuilds.count - before.band_rebuilds.count;
        let lr = m.stats.label_rebuilds.count - before.label_rebuilds.count;
        if br + lr > 0 {
            println!("  {:<30} band rebuilds {br}, label rebuilds {lr}", "");
        }
        b.dump(&label.replace(|c: char| !c.is_ascii_alphanumeric(), "_"))?;
        Ok(())
    };
    println!("scenarios:");
    run(&mut b, &mut ms_, "static fit view", &|_, m| {
        m.camera.center = fit_center;
        m.camera.zoom = fit_zoom;
    })?;
    run(&mut b, &mut ms_, "pan at fit zoom", &|t, m| {
        let a = std::f32::consts::TAU * t;
        m.camera.center = [fit_center[0] + 0.05 * bounds.width() * a.sin(), fit_center[1] + 0.05 * bounds.height() * a.cos()];
        m.camera.zoom = fit_zoom;
    })?;
    for mul in [8.0f32, 64.0] {
        run(&mut b, &mut ms_, &format!("pan at {mul}x"), &|t, m| {
            let a = std::f32::consts::TAU * t;
            let r = 0.4 * bounds.width() / mul;
            m.camera.center = [target[0] + r * a.sin(), target[1] + r * a.cos()];
            m.camera.zoom = (fit_zoom * mul).clamp(m.camera.min_zoom, m.camera.max_zoom);
        })?;
    }
    run(&mut b, &mut ms_, "zoom sweep 1x..512x..1x", &|t, m| {
        let s = 1.0 - (2.0 * t - 1.0).abs();
        let z = fit_zoom * (s * 9.0).exp2();
        let c = [fit_center[0] + (target[0] - fit_center[0]) * s.min(0.5) * 2.0, fit_center[1] + (target[1] - fit_center[1]) * s.min(0.5) * 2.0];
        m.camera.center = c;
        m.camera.zoom = z.clamp(m.camera.min_zoom, m.camera.max_zoom);
    })?;
    let st = ms_.stats;
    println!(
        "  LOD band rebuilds: {} (mean {:.2}, max {:.2} ms); band uploads {} (mean {:.2}, max {:.2} ms); label rebuilds {} (mean {:.2}, max {:.2} ms)",
        st.band_rebuilds.count,
        ms(st.band_rebuilds.mean()),
        ms(st.band_rebuilds.max),
        st.band_uploads.count,
        ms(st.band_uploads.mean()),
        ms(st.band_uploads.max),
        st.label_rebuilds.count,
        ms(st.label_rebuilds.mean()),
        ms(st.label_rebuilds.max)
    );
    let mut rng = Rng::new(1);
    for (label, mul) in [("fit", 1.0f32), ("8x", 8.0), ("64x", 64.0)] {
        ms_.camera.center = if mul == 1.0 { fit_center } else { target };
        ms_.camera.zoom = (fit_zoom * mul).clamp(ms_.camera.min_zoom, ms_.camera.max_zoom);
        b.frame(&mut ms_)?;
        let n = 20_000;
        let mut hits = 0u32;
        let mut worst = Duration::ZERO;
        let t = Instant::now();
        for _ in 0..n {
            let s = [rng.f32() * SIZE[0] as f32, rng.f32() * SIZE[1] as f32];
            let t1 = Instant::now();
            hits += u32::from(ms_.hit_test(s, 3.0 * PPP).is_some());
            worst = worst.max(t1.elapsed());
        }
        println!("hit test at {label:>4}: mean {:.2} us, max {:.1} us ({hits}/{n} hits)", (t.elapsed() / n).as_secs_f64() * 1e6, worst.as_secs_f64() * 1e6);
    }
    let mem = b.renderer.canvas().map().memory();
    println!(
        "memory: scene {:.1} MiB CPU, code map {:.1} MiB, GPU buffers {:.1} MiB, atlas {:.1} MiB, peak RSS {:.1} MiB",
        mib(ms_.scene.heap_bytes()),
        mib(map.heap_bytes()),
        mib(mem.buffers as usize),
        mib(mem.atlas as usize),
        mib(process_usage().peak_rss_bytes as usize)
    );
    Ok(())
}
