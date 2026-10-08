//! 2D camera: pan and zoom over world space. The renderer uploads it as a 64-byte uniform;
//! moving the camera never touches instance buffers.
//!
//! Screen coordinates are physical pixels relative to the canvas top-left, y down. World
//! y also grows downwards. `screen = (world - center) * zoom + viewport / 2`.

use super::scene::{Band, WorldRect, band_for_zoom};

/// Precision budget: with f32 world coordinates up to `M` in magnitude, the shader's
/// `world - center` carries an error of about `M * 2^-24` world units. Zoom is capped so
/// that this stays under 1/8 px: `zoom <= 2^21 / M`.
const PRECISION_ZOOM_NUMERATOR: f32 = (1u32 << 21) as f32;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Camera {
    pub center: [f32; 2],
    /// Physical pixels per world unit.
    pub zoom: f32,
    /// Canvas size in physical pixels.
    pub viewport: [f32; 2],
    pub min_zoom: f32,
    pub max_zoom: f32,
}

impl Default for Camera {
    fn default() -> Self {
        Self { center: [0.0, 0.0], zoom: 1.0, viewport: [1.0, 1.0], min_zoom: 1e-30, max_zoom: 1e30 }
    }
}

impl Camera {
    pub fn world_to_screen(&self, p: [f32; 2]) -> [f32; 2] {
        [
            (p[0] - self.center[0]) * self.zoom + self.viewport[0] * 0.5,
            (p[1] - self.center[1]) * self.zoom + self.viewport[1] * 0.5,
        ]
    }

    pub fn screen_to_world(&self, s: [f32; 2]) -> [f32; 2] {
        [
            (s[0] - self.viewport[0] * 0.5) / self.zoom + self.center[0],
            (s[1] - self.viewport[1] * 0.5) / self.zoom + self.center[1],
        ]
    }

    /// The world rect currently on screen.
    pub fn visible(&self) -> WorldRect {
        let hw = self.viewport[0] * 0.5 / self.zoom;
        let hh = self.viewport[1] * 0.5 / self.zoom;
        WorldRect::new(self.center[0] - hw, self.center[1] - hh, self.center[0] + hw, self.center[1] + hh)
    }

    pub fn band(&self) -> Band {
        band_for_zoom(self.zoom)
    }

    /// Sets the canvas size, keeping centre and zoom. Non-positive sizes are ignored.
    pub fn set_viewport(&mut self, w: f32, h: f32) {
        if w > 0.0 && h > 0.0 && w.is_finite() && h.is_finite() {
            self.viewport = [w, h];
        }
    }

    /// Moves the view by a screen-space delta (dragging content right moves the camera left).
    pub fn pan_px(&mut self, dx: f32, dy: f32) {
        if !(dx.is_finite() && dy.is_finite()) {
            return;
        }
        self.center[0] -= dx / self.zoom;
        self.center[1] -= dy / self.zoom;
    }

    /// Multiplies zoom by `factor`, keeping the world point under `screen` fixed. The zoom
    /// is clamped to `[min_zoom, max_zoom]`.
    pub fn zoom_at(&mut self, screen: [f32; 2], factor: f32) {
        if !(factor.is_finite() && factor > 0.0) {
            return;
        }
        let anchor = self.screen_to_world(screen);
        self.zoom = (self.zoom * factor).clamp(self.min_zoom, self.max_zoom);
        // Re-centre so `anchor` maps back to `screen`.
        self.center[0] = anchor[0] - (screen[0] - self.viewport[0] * 0.5) / self.zoom;
        self.center[1] = anchor[1] - (screen[1] - self.viewport[1] * 0.5) / self.zoom;
    }

    /// Zoom limits for a scene with `bounds`: from a quarter of the fit zoom out to the f32
    /// precision limit (and at least the fit zoom).
    pub fn set_limits(&mut self, bounds: &WorldRect) {
        if !bounds.is_positive() {
            return;
        }
        let fit = self.fit_zoom(bounds);
        let m = bounds.min.iter().chain(&bounds.max).fold(1.0f32, |a, v| a.max(v.abs()));
        self.min_zoom = fit * 0.25;
        self.max_zoom = (PRECISION_ZOOM_NUMERATOR / m).max(fit);
        self.zoom = self.zoom.clamp(self.min_zoom, self.max_zoom);
    }

    fn fit_zoom(&self, bounds: &WorldRect) -> f32 {
        (self.viewport[0] / bounds.width()).min(self.viewport[1] / bounds.height())
    }

    /// Centres `bounds` and zooms so it fills `fill` (0..1] of the viewport.
    pub fn fit(&mut self, bounds: &WorldRect, fill: f32) {
        if !bounds.is_positive() {
            return;
        }
        self.center = bounds.center();
        self.zoom = (self.fit_zoom(bounds) * fill).clamp(self.min_zoom, self.max_zoom);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cam() -> Camera {
        Camera { center: [100.0, 50.0], zoom: 2.0, viewport: [800.0, 600.0], ..Camera::default() }
    }

    #[test]
    fn roundtrip_and_center() {
        let c = cam();
        assert_eq!(c.world_to_screen([100.0, 50.0]), [400.0, 300.0]);
        let p = [123.5, -7.25];
        let back = c.screen_to_world(c.world_to_screen(p));
        assert!((back[0] - p[0]).abs() < 1e-4 && (back[1] - p[1]).abs() < 1e-4);
        let v = c.visible();
        assert_eq!(v, WorldRect::new(-100.0, -100.0, 300.0, 200.0));
    }

    #[test]
    fn zoom_keeps_anchor_fixed() {
        let mut c = cam();
        let s = [123.0, 456.0];
        let w = c.screen_to_world(s);
        c.zoom_at(s, 3.0);
        assert_eq!(c.zoom, 6.0);
        let s2 = c.world_to_screen(w);
        assert!((s2[0] - s[0]).abs() < 1e-3 && (s2[1] - s[1]).abs() < 1e-3, "{s2:?}");
        // Invalid factors are ignored.
        let before = c;
        c.zoom_at(s, 0.0);
        c.zoom_at(s, f32::NAN);
        c.zoom_at(s, -2.0);
        assert_eq!(c, before);
    }

    #[test]
    fn pan_moves_content_with_the_pointer() {
        let mut c = cam();
        let w = [110.0, 60.0];
        let s = c.world_to_screen(w);
        c.pan_px(30.0, -10.0);
        let s2 = c.world_to_screen(w);
        assert!((s2[0] - s[0] - 30.0).abs() < 1e-3 && (s2[1] - s[1] + 10.0).abs() < 1e-3);
        c.pan_px(f32::INFINITY, 0.0);
        assert!(c.center[0].is_finite());
    }

    #[test]
    fn fit_and_limits() {
        let mut c = Camera { viewport: [1000.0, 500.0], ..Camera::default() };
        let b = WorldRect::new(0.0, 0.0, 100.0, 100.0);
        c.set_limits(&b);
        c.fit(&b, 1.0);
        assert_eq!(c.zoom, 5.0);
        assert_eq!(c.center, [50.0, 50.0]);
        assert_eq!(c.min_zoom, 1.25);
        assert_eq!(c.max_zoom, (1u32 << 21) as f32 / 100.0);
        c.zoom_at([0.0, 0.0], 1e-9);
        assert_eq!(c.zoom, c.min_zoom);
        c.zoom_at([0.0, 0.0], 1e30);
        assert_eq!(c.zoom, c.max_zoom);
        // Degenerate bounds and viewports are ignored.
        let before = c;
        c.fit(&WorldRect::EMPTY, 1.0);
        c.set_limits(&WorldRect::new(0.0, 0.0, 0.0, 5.0));
        c.set_viewport(0.0, 10.0);
        c.set_viewport(f32::NAN, 10.0);
        assert_eq!(c, before);
    }

    #[test]
    fn band_tracks_zoom() {
        let mut c = cam();
        c.zoom = 1.0;
        assert_eq!(c.band(), 128);
        c.zoom = 1.99;
        assert_eq!(c.band(), 128);
        c.zoom = 2.0;
        assert_eq!(c.band(), 129);
        c.zoom = 0.5;
        assert_eq!(c.band(), 127);
    }
}
