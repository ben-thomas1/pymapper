//! The canvas: the window area between the egui panels, drawn by our own wgpu pipelines
//! before egui so egui never touches it. It clears to the background and draws the code
//! map ([`crate::map`]) with the contract `prepare` (update + upload only what changed)
//! then `draw` (scissored to the canvas).

use crate::map::MapState;
use crate::map::gpu::MapRenderer;

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
}

impl CanvasStyle {
    pub const DEFAULT: Self = Self { background: [0.075, 0.078, 0.086, 1.0] };
}

/// The canvas: background clear plus the instanced map renderer.
pub struct Canvas {
    map: MapRenderer,
    style: CanvasStyle,
    srgb_target: bool,
}

impl Canvas {
    pub fn new(device: &wgpu::Device, format: wgpu::TextureFormat) -> Self {
        Self { map: MapRenderer::new(device, format), style: CanvasStyle::DEFAULT, srgb_target: format.is_srgb() }
    }

    /// Clear colour for the frame (the canvas background), encoded for the target format.
    pub fn clear_color(&self) -> wgpu::Color {
        let [r, g, b, a] = encode(self.style.background, self.srgb_target);
        wgpu::Color { r: f64::from(r), g: f64::from(g), b: f64::from(b), a: f64::from(a) }
    }

    pub fn map(&self) -> &MapRenderer {
        &self.map
    }

    /// Updates the map for this frame and uploads what changed (see [`MapRenderer::prepare`]).
    pub fn prepare(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, rect: PhysicalRect, target: [u32; 2], ppp: f32, map: &mut MapState) {
        self.map.prepare(device, queue, map, rect, target, ppp);
    }

    /// Draws into `rect` of the current pass. `rect` must lie within the pass's target.
    pub fn draw(&self, pass: &mut wgpu::RenderPass<'_>, rect: PhysicalRect) {
        self.map.draw(pass, rect);
    }
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
    fn srgb_target_decodes_to_linear() {
        assert_eq!(encode([0.5, 0.5, 0.5, 0.7], false), [0.5, 0.5, 0.5, 0.7]);
        let lin = encode([0.0, 0.5, 1.0, 0.7], true);
        assert_eq!(lin[0], 0.0);
        assert!((lin[1] - 0.214).abs() < 1e-3, "{}", lin[1]);
        assert!((lin[2] - 1.0).abs() < 1e-6);
        assert_eq!(lin[3], 0.7, "alpha stays linear");
    }
}
