//! pymapper native GUI shell (design: GUI).
//!
//! winit owns the window and event loop, wgpu owns the GPU (Metal on macOS, Vulkan on
//! Linux), and egui draws only the side panels, sharing wgpu's device and queue. The
//! centre of the window is the canvas, drawn by our own instanced renderer ([`map`])
//! underneath egui.
//!
//! The loop renders on demand: nothing is drawn and the thread sleeps in the OS event
//! wait until input arrives or egui asks for a repaint ([`schedule`]). An idle window
//! costs no CPU.
//!
//! ```text
//! main.rs     args -> EventLoop -> App
//! app.rs      winit ApplicationHandler: window, surface, egui input, redraw scheduling
//! gpu.rs      instance / adapter / device / surface configuration (backend per OS)
//! render.rs   one frame: canvas pass + egui pass into a texture view
//! canvas.rs   canvas: background + map renderer, logical -> physical pixel rects
//! map/        instanced code-map renderer: scene, grid, LOD bands, camera, text, GPU
//! codemap/    the code map: in-process analysis, semantic zoom, bundled edges, scenes
//! views/      secondary views linked by selection: force graph, dependency matrix, focus
//! mapui.rs    code map controls and legend (egui)
//! panels.rs   egui panel layout (left search/tree, right details, bottom findings)
//! schedule.rs render-on-demand scheduler (pure)
//! stats.rs    frame-time samples, process CPU time / peak RSS
//! measure.rs  `--measure`: frame time + idle CPU report
//! ```

pub mod app;
pub mod args;
pub mod canvas;
pub mod codemap;
pub mod gpu;
pub mod map;
pub mod mapui;
pub mod measure;
pub mod panels;
pub mod render;
pub mod schedule;
pub mod stats;
pub mod views;
