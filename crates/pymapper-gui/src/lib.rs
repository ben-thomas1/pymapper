//! pymapper native GUI shell (design: GUI).
//!
//! winit owns the window and event loop, wgpu owns the GPU (Metal on macOS, Vulkan on
//! Linux), and egui draws only the side panels, sharing wgpu's device and queue. The
//! centre of the window is the canvas, drawn by our own pipeline ([`canvas`]) underneath
//! egui; for now it is a placeholder grid that the instanced code-map renderer replaces.
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
//! canvas.rs   canvas pipeline (placeholder grid) + logical -> physical pixel rects
//! panels.rs   egui panel layout (left search/tree, right details, bottom findings)
//! schedule.rs render-on-demand scheduler (pure)
//! stats.rs    frame-time samples, process CPU time / peak RSS
//! measure.rs  `--measure`: frame time + idle CPU report
//! ```

pub mod app;
pub mod args;
pub mod canvas;
pub mod gpu;
pub mod measure;
pub mod panels;
pub mod render;
pub mod schedule;
pub mod stats;
