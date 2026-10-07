//! egui panel layout. egui draws only these panels; the centre is left transparent for the
//! canvas, whose rect is reported back in [`UiState::canvas_rect`].
//!
//! ```text
//! +-----------+--------------------------+------------+
//! | search    |                          | details    |
//! | tree      |   canvas (own pipeline)  |            |
//! |           |                          |            |
//! |           +--------------------------+            |
//! |           | findings                 |            |
//! +-----------+--------------------------+------------+
//! ```

use egui::{Align2, FontId, Rect};

pub const LEFT_ID: &str = "pm_left";
pub const RIGHT_ID: &str = "pm_right";
pub const BOTTOM_ID: &str = "pm_bottom";

pub const LEFT_DEFAULT: f32 = 260.0;
pub const RIGHT_DEFAULT: f32 = 300.0;
pub const BOTTOM_DEFAULT: f32 = 170.0;

/// Per-window UI state. Built once; nothing here is reallocated per frame.
#[derive(Debug)]
pub struct UiState {
    pub search: String,
    /// Project root as displayed (computed once).
    pub project: String,
    /// GPU adapter / backend summary (computed once).
    pub renderer_info: String,
    /// Output: the canvas rect of the last frame, in points.
    pub canvas_rect: Rect,
}

impl UiState {
    pub fn new(project: String, renderer_info: String) -> Self {
        Self { search: String::new(), project, renderer_info, canvas_rect: Rect::NOTHING }
    }
}

/// Lays out all panels for one frame and records the canvas rect.
pub fn show(ui: &mut egui::Ui, state: &mut UiState) {
    egui::Panel::left(LEFT_ID)
        .resizable(true)
        .default_size(LEFT_DEFAULT)
        .size_range(160.0..=640.0)
        .show(ui, |ui| left(ui, state));

    egui::Panel::right(RIGHT_ID)
        .resizable(true)
        .default_size(RIGHT_DEFAULT)
        .size_range(180.0..=720.0)
        .show(ui, |ui| right(ui, state));

    egui::Panel::bottom(BOTTOM_ID)
        .resizable(true)
        .default_size(BOTTOM_DEFAULT)
        .size_range(60.0..=600.0)
        .show(ui, bottom);

    let central = egui::CentralPanel::no_frame().show(ui, |ui| {
        let rect = ui.max_rect();
        ui.painter().text(
            rect.center(),
            Align2::CENTER_CENTER,
            "code map",
            FontId::proportional(14.0),
            ui.visuals().weak_text_color(),
        );
    });
    state.canvas_rect = central.response.rect;
}

fn left(ui: &mut egui::Ui, state: &mut UiState) {
    ui.add_space(4.0);
    ui.add(
        egui::TextEdit::singleline(&mut state.search)
            .hint_text("Search symbols")
            .desired_width(f32::INFINITY),
    );
    ui.separator();
    ui.strong("Project");
    ui.weak(&state.project);
    ui.add_space(4.0);
    egui::ScrollArea::vertical().id_salt("pm_tree").auto_shrink([false, false]).show(ui, |ui| {
        ui.weak("No project loaded.");
    });
}

fn right(ui: &mut egui::Ui, state: &UiState) {
    ui.add_space(4.0);
    ui.strong("Details");
    ui.weak("Nothing selected.");
    ui.separator();
    ui.collapsing("Renderer", |ui| {
        ui.monospace(&state.renderer_info);
    });
}

fn bottom(ui: &mut egui::Ui) {
    ui.add_space(4.0);
    ui.strong("Findings");
    egui::ScrollArea::vertical().id_salt("pm_findings").auto_shrink([false, false]).show(ui, |ui| {
        ui.weak("No findings.");
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::canvas::PhysicalRect;
    use egui::{RawInput, ViewportId, pos2, vec2};

    /// Runs `frames` UI passes on a `w` x `h` point screen at `ppp` and returns the state.
    fn layout(ctx: &egui::Context, state: &mut UiState, w: f32, h: f32, ppp: f32, frames: usize) {
        for _ in 0..frames {
            let mut raw = RawInput {
                screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(w, h))),
                ..RawInput::default()
            };
            raw.viewports.entry(ViewportId::ROOT).or_default().native_pixels_per_point = Some(ppp);
            let mut out = ctx.run_ui(raw, |ui| show(ui, state));
            out.textures_delta.clear();
            assert!((out.pixels_per_point - ppp).abs() < 1e-6);
        }
    }

    fn state() -> UiState {
        UiState::new("proj".to_owned(), "test".to_owned())
    }

    #[test]
    fn canvas_sits_between_panels() {
        let ctx = egui::Context::default();
        let mut s = state();
        layout(&ctx, &mut s, 1280.0, 800.0, 1.0, 3);
        let c = s.canvas_rect;
        assert!(c.is_positive(), "{c:?}");
        // Left and right panels take their default widths (plus separators/margins).
        assert!((LEFT_DEFAULT..LEFT_DEFAULT + 24.0).contains(&c.min.x), "{c:?}");
        assert!((1280.0 - RIGHT_DEFAULT - 24.0..=1280.0 - RIGHT_DEFAULT).contains(&c.max.x), "{c:?}");
        // No top panel; findings at the bottom.
        assert_eq!(c.min.y, 0.0);
        assert!((800.0 - BOTTOM_DEFAULT - 24.0..=800.0 - BOTTOM_DEFAULT).contains(&c.max.y), "{c:?}");
    }

    #[test]
    fn hidpi_keeps_point_layout_and_doubles_pixels() {
        let ctx1 = egui::Context::default();
        let mut s1 = state();
        layout(&ctx1, &mut s1, 1280.0, 800.0, 1.0, 3);
        let ctx2 = egui::Context::default();
        let mut s2 = state();
        layout(&ctx2, &mut s2, 1280.0, 800.0, 2.0, 3);
        let (a, b) = (s1.canvas_rect, s2.canvas_rect);
        assert!((a.min - b.min).length() < 1.0 && (a.max - b.max).length() < 1.0, "{a:?} vs {b:?}");
        let p1 = PhysicalRect::from_points(a, 1.0, [1280, 800]).unwrap();
        let p2 = PhysicalRect::from_points(b, 2.0, [2560, 1600]).unwrap();
        assert!(p2.w.abs_diff(2 * p1.w) <= 2 && p2.h.abs_diff(2 * p1.h) <= 2, "{p1:?} vs {p2:?}");
    }

    #[test]
    fn resize_moves_canvas_edges() {
        let ctx = egui::Context::default();
        let mut s = state();
        layout(&ctx, &mut s, 1280.0, 800.0, 1.0, 3);
        let before = s.canvas_rect;
        layout(&ctx, &mut s, 1600.0, 1000.0, 1.0, 2);
        let after = s.canvas_rect;
        // Side panels keep their width; the canvas absorbs the extra space.
        assert!((after.min.x - before.min.x).abs() < 1.0);
        assert!((after.width() - before.width() - 320.0).abs() < 1.0, "{before:?} -> {after:?}");
        assert!((after.height() - before.height() - 200.0).abs() < 1.0, "{before:?} -> {after:?}");
    }

    #[test]
    fn tiny_windows_do_not_panic_and_canvas_may_vanish() {
        let ctx = egui::Context::default();
        let mut s = state();
        for (w, h) in [(1.0, 1.0), (100.0, 80.0), (0.0, 0.0), (400.0, 300.0)] {
            layout(&ctx, &mut s, w, h, 2.0, 2);
            let target = [(w * 2.0) as u32, (h * 2.0) as u32];
            // Whatever egui decides, the physical rect is either None or inside the target.
            if let Some(p) = PhysicalRect::from_points(s.canvas_rect, 2.0, target) {
                assert!(p.x + p.w <= target[0] && p.y + p.h <= target[1], "{p:?} {target:?}");
            }
        }
    }

    #[test]
    fn idle_ui_requests_no_repaint() {
        // Render on demand relies on egui settling: after the first frames, an idle UI
        // must not ask for another frame.
        let ctx = egui::Context::default();
        let mut s = state();
        let mut last = None;
        for _ in 0..4 {
            let raw = RawInput {
                screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(1280.0, 800.0))),
                ..RawInput::default()
            };
            let mut out = ctx.run_ui(raw, |ui| show(ui, &mut s));
            out.textures_delta.clear();
            last = Some(out.viewport_output[&ViewportId::ROOT].repaint_delay);
        }
        assert_eq!(last, Some(std::time::Duration::MAX));
    }
}
