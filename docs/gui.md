# pymapper-gui

Native GUI shell (`crates/pymapper-gui`, binary `pymapper-gui`). winit owns the window
and event loop. wgpu owns the GPU. egui draws only the side panels and shares wgpu's device
and queue through egui-wgpu. The centre of the window is the canvas, drawn by our own
pipeline. It is a placeholder grid for now; the instanced code-map renderer replaces it.

The CLI (`pymapper`) does not depend on any GPU crate. `pymapper-gui` is a workspace member
but not a default member, so build it explicitly:

```
cargo build --release -p pymapper-gui      # target/release/pymapper-gui
cargo run -p pymapper-gui -- [PATH]        # debug build; wgpu validation on
cargo test -p pymapper-gui
```

## Layout

```
+-----------+--------------------------+------------+
| search    |                          | details    |
| tree      |   canvas (own pipeline)  |            |
|           |                          |            |
|           +--------------------------+            |
|           | findings                 |            |
+-----------+--------------------------+------------+
```

All three panels can be resized. The canvas takes whatever space is left. egui reports the
canvas rect in points each frame, and `PhysicalRect::from_points` converts it to framebuffer
pixels. The conversion rounds edges rather than sizes, so a panel and the canvas meet on the
same pixel at any scale factor (1x, 1.25x, 1.5x, 2x).

## Modules

```
crates/pymapper-gui/src/
  main.rs       args -> EventLoop -> App; exit codes 0 / 2
  app.rs        winit ApplicationHandler: window, surface lifecycle, egui input, redraws
  gpu.rs        instance / adapter / device / surface configuration (backend per OS)
  render.rs     one frame: clear + canvas + egui in a single render pass
  canvas.rs     canvas pipeline (placeholder grid), logical -> physical pixel rects
  canvas.wgsl   placeholder grid shader
  panels.rs     egui panel layout (left search/tree, right details, bottom findings)
  schedule.rs   render-on-demand scheduler (pure, unit-tested)
  stats.rs      frame-time ring buffer + percentiles, process CPU time / peak RSS
  measure.rs    --measure: frame time + idle CPU report
  args.rs       hand-written argument parser (clap is reserved for the CLI)
crates/pymapper-gui/tests/offscreen.rs
                renders the full UI into an offscreen texture and checks pixels
```

## Render on demand

The loop never redraws on a timer. It sleeps in the OS event wait (`ControlFlow::Wait`)
until one of these happens:

- An input event that egui says needs a repaint (`egui_winit::State::on_window_event`).
- egui asks for a later frame after drawing one (`repaint_delay`: zero while animating,
  a finite delay for tooltips or the cursor blink, `Duration::MAX` when idle). The
  scheduler then uses `ControlFlow::WaitUntil` for that deadline only.
- Another thread calls `egui::Context::request_repaint`, which will be the layout thread
  later. egui's repaint callback posts a `UserEvent::Repaint` through the event-loop proxy.
  The event is dropped if a frame has finished since the request was made, because that
  frame's `repaint_delay` already covered it.
- Resize, scale-factor change, or the window becoming visible again (`Occluded(false)`).

The surface uses vsync (`PresentMode::AutoVsync`, frame latency 2). Because frames are
only drawn on demand, vsync costs nothing while the window is idle. Covered or minimised
windows draw nothing.

The window is created hidden and shown after the first frame is presented, so it never
flashes blank. On macOS a hidden window has no drawable, and wgpu reports `Occluded`. In
that case the app shows the window and retries once. Wayland ignores the hidden flag: a
Wayland window maps on its first commit.

Surface errors never abort the app. `Timeout` retries. `Outdated` and `Suboptimal`
reconfigure from the window's *current* size (not the last configured one, which could stay
outdated forever). `Lost` recreates the surface. `Validation` drops the frame, because wgpu
has already sent the error to its handler.

## HiDPI and resize

- egui-winit gets the native scale factor at startup and on `ScaleFactorChanged`, so
  panels are laid out in points and rasterised at the native resolution.
- The surface is configured in physical pixels. It is clamped to the device's maximum
  texture dimension, and left unconfigured while the window has zero area (minimised). The
  device requests `Limits::default().using_resolution(adapter.limits())`, so large HiDPI
  displays work.
- The canvas shader works in physical pixels: grid spacing and line width are multiplied by
  the scale factor.
- `Resized` reconfigures the surface and calls `request_redraw` directly, because the loop
  may not reach `about_to_wait` during a macOS live resize.

## Platforms

| OS    | Backend | Window system                         | Status                          |
|-------|---------|---------------------------------------|---------------------------------|
| macOS | Metal   | AppKit                                | built, tested, measured         |
| Linux | Vulkan  | Wayland first, X11/XWayland fallback  | compiles (cfg-gated), not run   |

The per-OS choices live in `crates/pymapper-gui/Cargo.toml` (target-specific features) and
`gpu::BACKENDS`:

- macOS: `wgpu/metal`. winit uses no extra features.
- Linux: `wgpu/vulkan`. winit uses `wayland`, `wayland-dlopen`,
  `wayland-csd-adwaita-notitle`, `x11`, and egui-winit uses `wayland`, `x11`. The
  Wayland, X11 and xkb client libraries are loaded with dlopen at runtime, so a build needs
  no system `-dev` packages. At runtime it needs the Vulkan loader plus a driver
  (`libvulkan.so.1`), and either `libwayland-client.so` + `libxkbcommon.so` or the X11
  libraries. winit picks Wayland when `WAYLAND_DISPLAY` (or `WAYLAND_SOCKET`) is set, and
  X11 via `DISPLAY` otherwise. To force X11 under Wayland, run with `WAYLAND_DISPLAY=`
  unset. With `wayland-csd-adwaita-notitle`, compositors without server-side
  decorations (GNOME) draw client-side decorations without a title text, which avoids a
  font-rendering dependency.
- Other targets fall back to wgpu's primary backends. They are unsupported.

To check that the Linux paths compile from macOS (the target only needs `rust-std`):

```
rustup target add aarch64-unknown-linux-gnu
cargo check  -p pymapper-gui --all-targets --target aarch64-unknown-linux-gnu
cargo clippy -p pymapper-gui --all-targets --target aarch64-unknown-linux-gnu -- -D warnings
```

`ru_maxrss` is in bytes on macOS and in KiB on Linux. `stats::process_usage` normalises it
to bytes.

Environment overrides (all read by wgpu): `WGPU_BACKEND` (for example `metal` or `vulkan`),
`WGPU_POWER_PREF` (`low`, the default because the map is 2D, or `high`), and `WGPU_DEBUG` /
`WGPU_VALIDATION` for instance flags. Validation is on in debug builds and off in release.

## Measuring: `pymapper-gui --measure`

```
pymapper-gui --measure [--frames N] [--idle SECS] [PATH]
```

The measurement runs in four phases:

1. 30 warm-up frames, which are not recorded (shader compilation, font atlas upload).
2. N timed frames (default 300), with a redraw forced every frame.
3. A 0.5 s settle period, so egui can finish any follow-up repaints.
4. An idle window (default 5 s) where nothing touches the window.

It then prints a report and exits (code 0). Measuring needs a window that can present.
The window floats above other windows in this mode, but it cannot cross Spaces, so a
full-screen app on another Space still occludes it. If no frame is presented for 10 s while
frames are forced, the run stops with exit code 2 and says so rather than waiting forever.

Per-frame CPU times are split into:

- `ui`: egui input, layout and tessellation.
- `upload+encode+submit+present`.
- `cpu work`: the sum of the two above.
- `swapchain acquire`: waiting for a drawable. This includes vsync back-pressure, so it is
  not work.

The idle line shows process CPU time across all threads (`getrusage`), frames rendered and
event-loop wakeups during the idle window. Run it on a release build. Moving the mouse over
the window during the idle window counts as wakeups and frames.

GPU execution time is not measured separately. It would need timestamp queries, which add
a device feature requirement; the instanced renderer step can add them.

### Results (macOS, shell with placeholder canvas)

Release build, Apple M4 Pro (Metal), 120 Hz display, surface 2880x1800 px at 2x:

| frames | cpu work p50 / p99 / max | ui p50   | acquire p50 (vsync) | fps   | idle CPU             | idle frames | peak RSS  |
|--------|--------------------------|----------|---------------------|-------|----------------------|-------------|-----------|
| 300    | 0.070 / 0.222 / 0.265 ms | 0.016 ms | 8.24 ms             | 120.0 | 0.5 ms in 5 s (0.010%) | 0         | 110.2 MiB |
| 600    | 0.127 / 0.273 / 0.310 ms | 0.031 ms | 8.19 ms             | 120.0 | 0.4 ms in 10 s (0.004%) | 0        | 105.2 MiB |

Launch to first frame presented: 140-200 ms over several runs. Across runs, idle CPU was
0.3-0.5 ms per 5-10 s window, with 0 frames rendered and 1 event wakeup. Peak RSS was
96-111 MiB.

Measured from outside the process (`ps`), a release window left idle for 30 s accumulated
at most 10 ms of CPU time, which is the resolution of `ps`.

Where the memory goes (`vmmap --summary` on an idle window at 2880x1800):

- 60.5 MB `IOSurface`, which is the swapchain drawables (about 20 MB each at this size).
- About 16 MB malloc (egui font atlas, tessellation buffers, wgpu bookkeeping).
- The rest is dirty `__DATA` of system frameworks (Metal, AppKit).

Lowering `FRAME_LATENCY` to 1, which sets the CAMetalLayer drawable count to 2, did not
change the IOSurface footprint in a test, so it stays at 2 (the wgpu default).

## Dependencies

Only the GUI crates the dependency policy allows: winit, wgpu, egui, egui-wgpu, egui-winit
and pollster, plus anyhow and libc (getrusage). Default features are off. Each crate gets
only what it needs:

- wgpu: `std`, `parking_lot`, `wgsl`, plus a backend per OS.
- egui: `default_fonts`.
- egui-winit: `clipboard`, so copy and paste work in text fields.
- winit: `rwh_06`.

Known transitive cost: egui-winit's `clipboard` feature enables `arboard` with
`image-data`, which pulls in the `image` crate (png/tiff decoders) for image clipboard
support we do not use. This affects binary size and build time, not runtime memory, and
egui-winit does not expose a switch for it. The alternative is our own NSPasteboard /
wl-clipboard layer, which is a follow-up if binary size matters.
