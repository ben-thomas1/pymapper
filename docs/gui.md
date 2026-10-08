# pymapper-gui

Native GUI shell (`crates/pymapper-gui`, binary `pymapper-gui`). winit owns the window
and event loop. wgpu owns the GPU. egui draws only the side panels and shares wgpu's device
and queue through egui-wgpu. The centre of the window is the canvas, drawn by our own
instanced renderer (`src/map/`, see [Canvas renderer](#canvas-renderer)).

The CLI (`pymapper`) does not depend on any GPU crate. `pymapper-gui` is a workspace member
but not a default member, so build it explicitly:

```
cargo build --release -p pymapper-gui      # target/release/pymapper-gui
cargo run -p pymapper-gui -- [PATH]        # debug build; wgpu validation on
cargo run --release -p pymapper-gui -- --stress 1000000   # synthetic 1M rects + 1M edges
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
  canvas.rs     canvas: background clear + map renderer, logical -> physical pixel rects
  map/          instanced code-map renderer (see "Canvas renderer")
  panels.rs     egui panel layout (left search/tree, right details, bottom findings),
                canvas input (pan / zoom / hover)
  schedule.rs   render-on-demand scheduler (pure, unit-tested)
  stats.rs      frame-time ring buffer + percentiles, process CPU time / peak RSS
  measure.rs    --measure: frame time + idle CPU report
  args.rs       hand-written argument parser (clap is reserved for the CLI)
crates/pymapper-gui/tests/offscreen.rs
                renders the UI and the map into an offscreen texture and checks pixels
crates/pymapper-gui/examples/canvas_bench.rs
                offscreen frame-time benchmark of the map on the stress scene
```

## Canvas renderer

`src/map/` draws the code map. egui never touches it.

```
map/
  scene.rs   GPU instance structs, LOD bands, SceneBuilder -> Scene (sorted by layer, grid bin)
  grid.rs    own spatial grid: a short pyramid of uniform grids (culling + hit testing)
  band.rs    band-filtered slots + per-bin slot tables, rebuilt only on LOD change
  camera.rs  pan / zoom, world <-> screen, zoom limits (f32 precision budget)
  hit.rs     exact hit tests (rounded rects, Bezier ribbons) over grid candidates
  text.rs    fonts + rasterisation (glyphon's cosmic-text), own glyph atlas packer, label layout
  gpu.rs     wgpu pipelines, static instance buffers, atlas texture, camera uniform, draw list
  stress.rs  deterministic synthetic scene (packages -> modules -> functions + curves)
  map.wgsl   all shaders; one 48-byte camera uniform shared by every pipeline
```

### What is drawn

| primitive    | instance | draw                                   | shading                                   |
|--------------|----------|----------------------------------------|-------------------------------------------|
| rect         | 32 B     | 4-vertex strip per instance            | SDF rounded box: fill + inner stroke, AA  |
| line         | 40 B     | 4-vertex strip per instance            | ribbon, distance-to-centre AA             |
| curve        | 40 B     | 34-vertex strip (16 segments) in the VS | cubic Bezier ribbon of constant width     |
| glyph        | 32 B     | 4-vertex strip per instance            | `textureLoad` from an R8 alpha atlas      |

Rect radius, stroke width and edge width are in points and scaled by the DPI factor in the
shader. Colours are gamma-space RGBA8 (like egui), output premultiplied; for `*Srgb` targets
the shader decodes them first. Layers draw in ascending order (rect layers, then lines, then
curves, then labels, then the hover highlight). Items in one layer should not overlap
(true for a treemap level), because their order inside the layer is the grid order.

### Camera and LOD (no CPU work while navigating)

- The camera (centre, zoom, canvas origin, target size, text metrics, DPI) is a uniform.
  Panning and zooming rewrite those 48 bytes and nothing else.
- Zoom is grouped into bands: `band = floor(log2(zoom)) + 128`, where zoom is physical pixels
  per world unit, so each band spans a factor of 2. Every item has a `Lod { min, max }`
  band range. Rects get one automatically: visible from the first band in which they are at
  least 2 px (`MIN_RECT_PX`). Callers set explicit ranges for semantic zoom (e.g. rolled-up
  edges only at coarse bands).
- Per batch (rects, lines, curves), the set of visible items is rebuilt only when the camera
  crosses a band where that batch's membership changes (a 256-bit bitmap of change points).
  The rebuild is one linear filter pass over the batch, which is stored in grid order, and
  the batches are filtered concurrently. Its result is copied straight into wgpu's staging
  memory, one `memcpy` per run of consecutive items. Instance buffers are sized to fit
  (64 KiB granules) and shrink when less than a quarter used.
- Labels are rebuilt when the band changes, the DPI changes, or the view leaves the label
  region (the view plus one view of margin on each side). Panning inside the region costs
  nothing.

### Spatial grid (culling and hit testing)

One grid pyramid per layer: level 0 is uniform with about 8 items per cell (at most 1024
cells per axis). Each level above has cells 4 times larger, up to a single cell. An item
goes into exactly one bin: the cell holding its bbox centre, at the finest level whose cells
are at least as large as the item (a loose grid). A query therefore expands its rect by
half a cell per level. Nothing is stored twice and nothing is scanned linearly.

Bins are numbered level by level, row-major. Instances are sorted by bin, so the items of
one row of cells form one contiguous instance range:

- Culling: per frame, each layer's visible cells turn into instance ranges. Ranges
  are merged across the smallest gaps until there are at most 16 per layer
  (`limit_ranges`, which is optimal for the extra instances it draws). They are then issued
  as `draw(0..verts, start..end)` over the static buffer. Off-screen instances inside a
  range are clipped by the GPU.
- Hit testing: the same query for the cursor (plus tolerance) yields candidates. Exact
  tests follow. Edges come first, to within 3 pt of the ribbon. They are rejected by
  bbox, then by fat line (the chord strip bounded by the control points), then by pruned
  de Casteljau subdivision down to exactly the 16 segments the shader draws. Rects come
  next, from the top layer down, with the corner radius taken into account. Candidate
  scans above 16k edges run in parallel (rayon). Hover hit tests are skipped when
  pointer, camera and bands are unchanged.

Long edges sit in coarse levels, so in a dense "hairball" of long edges most of them are
candidates for any point. That cost is inherent to bbox indexing. With 1M random-ish edges
it is the slowest path (see the table below). Bundled and rolled-up edges (S7.4) keep it
small.

### Labels

Fonts come from egui's built-in font bytes, loaded into a cosmic-text `FontSystem` (via
glyphon) without a system font scan, which keeps startup fast and memory low. Glyphs are
rasterised by swash at the label size, 4 horizontal subpixel bins, into an own R8 atlas
(shelf packer, 1024^2, doubles when full). Layout is a pen walk over advances. There is
no shaping, because labels are short identifiers. ASCII glyphs are found in a flat table.

Each glyph instance carries the labelled rect, so the vertex shader places the label at the
rect's top-left (snapped to whole pixels) and applies label LOD per frame:

- The rect must be tall enough for one line.
- The rect must be wide enough for the label's first 4 characters (or all of a shorter
  label).
- Glyphs that would cross the rect's right edge are dropped, which truncates the label.

The CPU only chooses which rects get labels. Those are rects in the band, inside the label
region, that can fit a label somewhere in the band. At most 16 384 are kept, largest first.

### Memory

Per item: 32 B (rect) or 40 B (edge) instance, 2 B LOD, 4 B id, plus 8 B label span and the
label text for rects. Grid bin tables add about 0.5 B per item. Band slots add 4 B per
visible item. The GPU holds only the visible band. The 1M + 1M stress scene is 102 MiB of
CPU scene data. The GPU buffers peak at 73 MiB when everything is in the band.

### API for the code map (S7.4)

```rust
let mut b = SceneBuilder::with_capacity(rects, edges);
b.add_rect(id, layer, WorldRect::new(x0, y0, x1, y1), RectStyle { fill, stroke, radius, stroke_width }, Some(label));
b.add_curve(id, [p0, c0, c1, p1], color, width_pt, Lod { min, max });
let mut map = MapState::new(Arc::new(b.build()));   // or map.set_scene(...)
// per frame: map.apply_input(&ui.canvas_input, ppp); renderer.render(.., FrameInput { map: &mut map, .. })
```

World coordinates should stay within about +-2^14. The camera caps zoom at `2^21 / max|coord|`
so f32 error stays under 1/8 px.

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

`--measure` does not measure GPU execution time separately. That would need timestamp
queries, which add a device feature requirement. The offscreen `canvas_bench` example
measures CPU and GPU time together by waiting for the GPU after each frame (see below).

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

### Map renderer results (1M rects + 1M edges)

`--stress N` replaces the empty map with the synthetic scene (N rects in 100 packages x 100
modules x ~99 functions, N Bezier edges: 70% local, 20% within a package range, 10% global).
With `--measure`, the timed frames follow a scripted path: zoom from fit to 128x and back
while drifting.

Release build, Apple M4 Pro (Metal), 2880x1800 px at 2x, 120 Hz:

```
pymapper-gui --measure --stress 1000000 --frames 600
timed frames: 600 (116.9 fps, vsync-bound)
  cpu work (ui + submit)   mean 0.879  p50 0.646  p95 2.701  p99 7.266  max 21.145 ms
idle: 5.0 s, cpu 0.8 ms (0.015% of one core), frames rendered 0, event wakeups 1
peak RSS: 410.8 MiB
scene build 100.6 ms; band rebuilds 9 (mean 1.95, max 4.05 ms);
band uploads 9 (mean 4.64, max 14.88 ms); label rebuilds 58 (mean 0.88, max 2.97 ms)
```

`cargo run --release -p pymapper-gui --example canvas_bench -- 1000000 1000000` renders
offscreen (no vsync, no window) and waits for the GPU after every frame. Frame time is
therefore the serialised CPU + GPU cost. 240 frames per scenario:

| scenario                             | frame p50 | p95      | max      | CPU p50  | instances drawn |
|--------------------------------------|-----------|----------|----------|----------|-----------------|
| fit view (natural LOD)               | 1.11 ms   | 1.27 ms  | 1.40 ms  | 0.055 ms | 71 200          |
| pan at fit zoom                      | 0.93 ms   | 1.00 ms  | 1.50 ms  | 0.071 ms | 71 200          |
| pan at 16x (grid culling)            | 7.01 ms   | 8.60 ms  | 9.49 ms  | 0.077 ms | 228 968         |
| pan at 128x (grid culling)           | 2.56 ms   | 3.16 ms  | 3.36 ms  | 0.074 ms | 135 583         |
| zoom sweep 1x -> 128x -> 1x          | 5.54 ms   | 18.4 ms  | 37.4 ms  | 0.080 ms | up to ~230k     |
| everything drawn, fit (2M instances) | 45.8 ms   | 48.9 ms  | 51.4 ms  | 0.079 ms | 2 000 990       |

Notes on the table:

- Scene build (generate + sort + grids) takes 108 ms. The first frame (fit, band build,
  labels, upload) takes 12 ms.
- Band rebuilds: mean 1.44 ms, max 2.39 ms. Band uploads: mean 3.8 ms, max 6.0 ms of CPU.
  Label rebuilds: mean 0.30 ms.
- In the zoom sweep, the max frame includes a band upload of up to 72 MiB.
- At 16x the frame time is GPU fill: about 10k long translucent edges cross the view.
  The forced worst case is fill-bound the same way, and the natural LOD never draws it.
- Hit test (20k random points): 2.9 us mean at fit, 351 us at 16x and 319 us at 128x (dense
  long edges, parallel scan). Rects alone take about 0.1 us. The first parallel scan
  spawns rayon's pool (one 22 ms outlier).
- Memory: scene 102 MiB CPU, GPU buffers 73 MiB peak, atlas 1 MiB.

At 100k + 100k the fit view already shows every edge (7.2 ms, fill-bound). Zoomed views
take 1-1.7 ms, and hit tests 117-194 us.

## Dependencies

Only the GUI crates the dependency policy allows: winit, wgpu, egui, egui-wgpu, egui-winit,
glyphon and pollster, plus anyhow, libc (getrusage) and rayon (parallel band filtering and
dense hit tests). Default features are off where a crate has any. Each crate gets only what
it needs:

- wgpu: `std`, `parking_lot`, `wgsl`, plus a backend per OS.
- egui: `default_fonts`.
- egui-winit: `clipboard`, so copy and paste work in text fields.
- winit: `rwh_06`.
- glyphon (0.12, same wgpu major): used for its cosmic-text re-export (font database,
  swash rasterisation). glyphon's own screen-space `TextRenderer` is not used. It re-prepares
  vertices whenever text moves, and the map needs world-anchored labels that cost nothing
  while the camera moves.

Known transitive cost: egui-winit's `clipboard` feature enables `arboard` with
`image-data`, which pulls in the `image` crate (png/tiff decoders) for image clipboard
support we do not use. This affects binary size and build time, not runtime memory, and
egui-winit does not expose a switch for it. The alternative is our own NSPasteboard /
wl-clipboard layer, which is a follow-up if binary size matters.
