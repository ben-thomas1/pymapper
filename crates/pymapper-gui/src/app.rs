//! The winit application: window + surface lifecycle, egui input, on-demand redraws.

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, anyhow};
use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::{StartCause, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoopProxy};
use winit::window::{Window, WindowId, WindowLevel};

use crate::args::Options;
use crate::canvas::PhysicalRect;
use crate::gpu;
use crate::map::{MapState, stress};
use crate::measure::{self, FrameTimes, Measure, STALL};
use crate::panels::{self, UiState};
use crate::render::{FrameInput, Renderer};
use crate::schedule::{Scheduler, Wake, earliest};

/// Events sent to the loop from other threads.
#[derive(Debug, Clone, Copy)]
pub enum UserEvent {
    /// egui wants a repaint after `delay` (sent by egui's repaint callback, e.g. from a
    /// background layout thread calling `Context::request_repaint`).
    Repaint { delay: Duration, pass_nr: u64 },
}

pub struct App {
    opts: Options,
    title: String,
    project: String,
    launched: Instant,
    egui_ctx: egui::Context,
    sched: Scheduler,
    measure: Option<Measure>,
    map: MapState,
    /// Time to build the scene (stress mode), for the measurement report.
    scene_build: Option<Duration>,
    win: Option<WindowState>,
    error: Option<anyhow::Error>,
    report: Option<String>,
}

struct WindowState {
    // Field order is drop order: GPU objects before the window they present to.
    renderer: Renderer,
    surface: wgpu::Surface<'static>,
    config: wgpu::SurfaceConfiguration,
    /// False while the window has zero area (minimised): the surface is not configured.
    configured: bool,
    max_dim: u32,
    queue: wgpu::Queue,
    device: wgpu::Device,
    adapter: wgpu::Adapter,
    instance: wgpu::Instance,
    egui: egui_winit::State,
    ui: UiState,
    occluded: bool,
    shown: bool,
    window: Arc<Window>,
}

impl App {
    /// `project` is the display form of the project root (computed once by the caller).
    pub fn new(opts: Options, project: String, launched: Instant, proxy: EventLoopProxy<UserEvent>) -> Self {
        let egui_ctx = egui::Context::default();
        // Repaint requests made outside a frame (other threads) must wake the loop.
        egui_ctx.set_request_repaint_callback(move |info| {
            let _ = proxy.send_event(UserEvent::Repaint { delay: info.delay, pass_nr: info.current_cumulative_pass_nr });
        });
        let measure = opts.measure.clone().map(|m| Measure::new(m, launched));
        let name = opts.path.file_name().map_or_else(|| project.clone(), |n| n.to_string_lossy().into_owned());
        let (map, scene_build) = match opts.stress {
            Some(n) => {
                let t = Instant::now();
                let scene = stress::scene(n as usize, n as usize, stress::SEED);
                (MapState::new(Arc::new(scene)), Some(t.elapsed()))
            }
            None => (MapState::default(), None),
        };
        Self {
            opts,
            title: format!("pymapper - {name}"),
            project,
            launched,
            egui_ctx,
            sched: Scheduler::new(),
            measure,
            map,
            scene_build,
            win: None,
            error: None,
            report: None,
        }
    }

    /// After the loop exits: the measurement report (if `--measure`) or the fatal error.
    pub fn finish(self) -> Result<Option<String>> {
        match self.error {
            Some(e) => Err(e),
            None => Ok(self.report),
        }
    }

    pub fn options(&self) -> &Options {
        &self.opts
    }

    fn fail(&mut self, event_loop: &ActiveEventLoop, err: anyhow::Error) {
        if self.error.is_none() {
            self.error = Some(err);
        }
        event_loop.exit();
    }

    fn create_window(&self, event_loop: &ActiveEventLoop) -> Result<WindowState> {
        let mut attrs = Window::default_attributes()
            .with_title(self.title.as_str())
            .with_inner_size(LogicalSize::new(1440.0, 900.0))
            .with_min_inner_size(LogicalSize::new(480.0, 320.0))
            // Shown after the first frame is presented, so it never flashes blank, where the
            // platform can present to a hidden window; see the `Occluded` arm in `redraw`.
            // (Ignored on Wayland, where a window maps on its first commit anyway.)
            .with_visible(false);
        if self.measure.is_some() {
            // Measuring needs a visible window: wgpu skips acquisition while it is occluded,
            // and a window opened from a background process may land behind the frontmost
            // app (macOS cooperative activation). Float it above other windows.
            attrs = attrs.with_window_level(WindowLevel::AlwaysOnTop);
        }
        let window = Arc::new(event_loop.create_window(attrs).context("creating window")?);
        let instance = gpu::instance();
        let surface = instance.create_surface(Arc::clone(&window)).context("creating GPU surface")?;
        let adapter = gpu::adapter(&instance, Some(&surface))?;
        let (device, queue) = gpu::device(&adapter)?;
        let max_dim = device.limits().max_texture_dimension_2d;
        let size = window.inner_size();
        let (w, h) = gpu::surface_extent(size.width, size.height, max_dim).unwrap_or((1, 1));
        let config = gpu::surface_config(&surface, &adapter, w, h)?;
        let configured = gpu::surface_extent(size.width, size.height, max_dim).is_some();
        if configured {
            surface.configure(&device, &config);
        }
        let renderer = Renderer::new(&device, config.format);
        let egui = egui_winit::State::new(
            self.egui_ctx.clone(),
            egui::ViewportId::ROOT,
            &*window,
            Some(window.scale_factor() as f32),
            window.theme(),
            Some(max_dim as usize),
        );
        let ui = UiState::new(self.project.clone(), gpu::describe(&adapter));
        Ok(WindowState {
            renderer,
            surface,
            config,
            configured,
            max_dim,
            queue,
            device,
            adapter,
            instance,
            egui,
            ui,
            occluded: false,
            shown: false,
            window,
        })
    }

    fn redraw(&mut self, event_loop: &ActiveEventLoop) {
        let Self { win, egui_ctx, sched, measure, launched, map, .. } = self;
        let Some(w) = win.as_mut() else { return };
        if w.occluded || !w.configured {
            // Dropped; un-occluding or resizing requests a new frame.
            return;
        }

        // Acquire first: if no image is available, skip before consuming egui input so
        // nothing (input, texture uploads) is lost.
        let t0 = Instant::now();
        let (frame, suboptimal) = match w.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(f) => (f, false),
            wgpu::CurrentSurfaceTexture::Suboptimal(f) => (f, true),
            wgpu::CurrentSurfaceTexture::Timeout => {
                w.window.request_redraw();
                return;
            }
            wgpu::CurrentSurfaceTexture::Occluded => {
                // A hidden window has no drawable (Metal reports it as occluded), so the
                // first frame could never be presented: show the window and retry once.
                // A shown but covered window is redrawn when winit reports Occluded(false).
                if !w.shown {
                    w.show();
                    w.window.request_redraw();
                }
                return;
            }
            wgpu::CurrentSurfaceTexture::Validation => {
                // wgpu has already reported the error through its handler; drop the frame.
                return;
            }
            wgpu::CurrentSurfaceTexture::Outdated => {
                // The window may have changed size before `Resized` arrived: reconfigure
                // from its current size, or the stale extent stays outdated forever.
                w.reconfigure();
                w.window.request_redraw();
                return;
            }
            wgpu::CurrentSurfaceTexture::Lost => {
                if let Err(e) = w.recreate_surface() {
                    self.fail(event_loop, e);
                    return;
                }
                w.window.request_redraw();
                return;
            }
        };
        let t1 = Instant::now();

        let raw = w.egui.take_egui_input(&w.window);
        w.ui.map_empty = !map.scene.bounds.is_positive();
        let mut full = egui_ctx.run_ui(raw, |ui| panels::show(ui, &mut w.ui));
        w.egui.handle_platform_output(&w.window, full.platform_output);
        let primitives = egui_ctx.tessellate(full.shapes, full.pixels_per_point);
        let hover_changed = map.apply_input(&w.ui.canvas_input, full.pixels_per_point);
        if let Some(t) = measure.as_ref().and_then(Measure::progress)
            && map.scene.bounds.is_positive()
        {
            // Scripted navigation for measurements: zoom 2^7 in and back while drifting.
            let fit = (map.camera.viewport[0] / map.scene.bounds.width()).min(map.camera.viewport[1] / map.scene.bounds.height());
            let (center, zoom) = stress::camera_path(t, &map.scene.bounds, fit, 7.0);
            map.camera.center = center;
            map.camera.zoom = zoom.clamp(map.camera.min_zoom, map.camera.max_zoom);
        }
        let t2 = Instant::now();

        let size = [w.config.width, w.config.height];
        let view = frame.texture.create_view(&wgpu::TextureViewDescriptor::default());
        let input = FrameInput {
            primitives: &primitives,
            textures: &mut full.textures_delta,
            size,
            pixels_per_point: full.pixels_per_point,
            canvas: PhysicalRect::from_points(w.ui.canvas_rect, full.pixels_per_point, size),
            map,
        };
        w.renderer.render(&w.device, &w.queue, &view, input);
        w.window.pre_present_notify();
        w.queue.present(frame);
        let t3 = Instant::now();

        if suboptimal {
            w.reconfigure();
        }
        if !w.shown {
            w.show();
        }
        let delay = full.viewport_output.get(&egui::ViewportId::ROOT).map_or(Duration::MAX, |v| v.repaint_delay);
        sched.frame_done(t3, delay);
        if !w.ui.map_empty {
            use std::fmt::Write as _;
            let st = &map.stats;
            w.ui.map_info.clear();
            let _ = write!(
                w.ui.map_info,
                "band {}  zoom {:.3}\n{} instances, {} draws\n{} labels, {} glyphs\nband rebuilds {}, label rebuilds {}",
                map.camera.band(),
                map.camera.zoom,
                st.drawn_instances,
                st.draw_calls,
                st.labels,
                st.glyphs,
                st.band_rebuilds.count,
                st.label_rebuilds.count
            );
        }
        if hover_changed {
            // The details panel was laid out before the hit test: show the new hover next frame.
            map.describe_hover(&mut w.ui.hover_text);
            sched.request(t3, Duration::ZERO);
        }
        if let Some(m) = measure.as_mut() {
            let times = FrameTimes { acquire: t1 - t0, ui: t2 - t1, submit: t3 - t2 };
            if m.on_frame(t3, times, t3 - *launched) {
                w.window.request_redraw();
            }
        }
    }
}

impl WindowState {
    fn show(&mut self) {
        self.window.set_visible(true);
        self.shown = true;
    }

    fn resize(&mut self, width: u32, height: u32) {
        match gpu::surface_extent(width, height, self.max_dim) {
            Some((w, h)) => {
                if !self.configured || self.config.width != w || self.config.height != h {
                    self.config.width = w;
                    self.config.height = h;
                    self.surface.configure(&self.device, &self.config);
                    self.configured = true;
                }
            }
            None => self.configured = false,
        }
    }

    /// Configures the surface for the window's current size, even if unchanged.
    fn reconfigure(&mut self) {
        let size = self.window.inner_size();
        self.configured = false;
        self.resize(size.width, size.height);
    }

    fn recreate_surface(&mut self) -> Result<()> {
        self.surface = self.instance.create_surface(Arc::clone(&self.window)).context("recreating GPU surface")?;
        self.config = gpu::surface_config(&self.surface, &self.adapter, self.config.width, self.config.height)?;
        if self.config.format != self.renderer.format() {
            self.renderer = Renderer::new(&self.device, self.config.format);
        }
        self.reconfigure();
        Ok(())
    }
}

impl ApplicationHandler<UserEvent> for App {
    fn new_events(&mut self, _event_loop: &ActiveEventLoop, cause: StartCause) {
        if let (StartCause::WaitCancelled { .. }, Some(m)) = (cause, self.measure.as_mut()) {
            m.on_event_wakeup();
        }
    }

    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.win.is_some() {
            return;
        }
        match self.create_window(event_loop) {
            Ok(w) => {
                w.window.request_redraw();
                self.win = Some(w);
            }
            Err(e) => self.fail(event_loop, e),
        }
    }

    fn user_event(&mut self, _event_loop: &ActiveEventLoop, event: UserEvent) {
        match event {
            UserEvent::Repaint { delay, pass_nr } => {
                // Requests made during a frame that has since completed were already
                // folded into that frame's `repaint_delay`.
                if pass_nr == self.egui_ctx.cumulative_pass_nr() {
                    self.sched.request(Instant::now(), delay);
                }
            }
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, id: WindowId, event: WindowEvent) {
        let Some(w) = self.win.as_mut() else { return };
        if w.window.id() != id {
            return;
        }
        if matches!(event, WindowEvent::RedrawRequested) {
            self.redraw(event_loop);
            return;
        }
        let response = w.egui.on_window_event(&w.window, &event);
        let now = Instant::now();
        if response.repaint {
            self.sched.request(now, Duration::ZERO);
        }
        match event {
            WindowEvent::CloseRequested | WindowEvent::Destroyed => event_loop.exit(),
            WindowEvent::Resized(size) => {
                w.resize(size.width, size.height);
                // Directly: during macOS live resize the loop may not reach about_to_wait.
                w.window.request_redraw();
            }
            WindowEvent::ScaleFactorChanged { .. } => {
                let size = w.window.inner_size();
                w.resize(size.width, size.height);
                w.window.request_redraw();
            }
            WindowEvent::Occluded(occluded) => {
                w.occluded = occluded;
                if !occluded {
                    self.sched.request(now, Duration::ZERO);
                }
            }
            _ => {}
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        let now = Instant::now();
        if self.measure.as_ref().is_some_and(|m| m.stalled(now)) {
            let err = anyhow!("--measure: no frame presented for {} s (window hidden or occluded?)", STALL.as_secs());
            self.fail(event_loop, err);
            return;
        }
        if let Some(m) = self.measure.as_mut()
            && m.poll(now)
        {
            if let Some(w) = &self.win {
                let mut report = m.report(&w.ui.renderer_info, [w.config.width, w.config.height], w.window.scale_factor() as f32);
                if self.map.scene.bounds.is_positive() {
                    measure::map_report(&mut report, &self.map, w.renderer.canvas().map().memory(), self.scene_build);
                }
                self.report = Some(report);
            }
            event_loop.exit();
            return;
        }
        if let (Wake::RedrawNow, Some(w)) = (self.sched.poll(now), &self.win) {
            w.window.request_redraw();
        }
        let deadline = earliest(self.sched.pending(), self.measure.as_ref().and_then(Measure::deadline));
        event_loop.set_control_flow(match deadline {
            Some(at) => ControlFlow::WaitUntil(at),
            None => ControlFlow::Wait,
        });
    }

    fn exiting(&mut self, _event_loop: &ActiveEventLoop) {
        // Release GPU resources while the window still exists.
        self.win = None;
    }
}
