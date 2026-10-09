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

use egui::{Align2, Event, FontId, Key, MouseWheelUnit, PointerButton, Rect, Sense};

use crate::codemap::scene::Legend;
use crate::codemap::worker::Request;
use crate::interact::panel::{self, PanelState};
use crate::interact::state::Interaction;
use crate::map::CanvasInput;
use crate::mapui;

pub const LEFT_ID: &str = "pm_left";
pub const RIGHT_ID: &str = "pm_right";
pub const BOTTOM_ID: &str = "pm_bottom";

pub const LEFT_DEFAULT: f32 = 260.0;
pub const RIGHT_DEFAULT: f32 = 300.0;
pub const BOTTOM_DEFAULT: f32 = 170.0;

/// Per-window UI state. Built once; nothing here is reallocated per frame.
pub struct UiState {
    /// Project root as displayed (computed once).
    pub project: String,
    /// GPU adapter / backend summary (computed once).
    pub renderer_info: String,
    /// Output: the canvas rect of the last frame, in points.
    pub canvas_rect: Rect,
    /// Output: pointer/keyboard input over the canvas during the last frame.
    pub canvas_input: CanvasInput,
    /// Input: whether the map has content (otherwise a placeholder hint is shown).
    pub map_empty: bool,
    /// Input: one-line description of the hovered map item (empty: nothing hovered).
    pub hover_text: String,
    /// Input: renderer statistics line for the details panel.
    pub map_info: String,
    /// In/out: the code map options (`None`: no code map, e.g. the stress scene). The
    /// controls edit it in place.
    pub map_request: Option<Request>,
    /// Output: the user changed `map_request` this frame.
    pub map_request_changed: bool,
    /// Input: legend of the scene shown.
    pub legend: Option<Legend>,
    /// Input: code map status over the canvas (loading phase or error; empty: none).
    pub map_status: String,
    /// In/out: hover, selection, search, path, overlays (see [`crate::interact`]).
    pub interact: Interaction,
    /// egui caches of the interaction panels.
    pub panel: PanelState,
}

impl std::fmt::Debug for UiState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UiState").field("canvas_rect", &self.canvas_rect).field("map_empty", &self.map_empty).finish_non_exhaustive()
    }
}

impl UiState {
    pub fn new(project: String, renderer_info: String) -> Self {
        Self {
            project,
            renderer_info,
            canvas_rect: Rect::NOTHING,
            canvas_input: CanvasInput::NONE,
            map_empty: true,
            hover_text: String::new(),
            map_info: String::new(),
            map_request: None,
            map_request_changed: false,
            legend: None,
            map_status: String::new(),
            interact: Interaction::default(),
            panel: PanelState::default(),
        }
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
        .show(ui, |ui| bottom(ui, state));

    let central = egui::CentralPanel::no_frame().show(ui, |ui| {
        let rect = ui.max_rect();
        let response = ui.interact(rect, ui.id().with("pm_canvas"), Sense::click_and_drag());
        state.canvas_input = canvas_input(ui, &response, rect);
        if !state.map_status.is_empty() {
            mapui::status(ui, rect, &state.map_status);
        } else if state.map_empty {
            ui.painter().text(
                rect.center(),
                Align2::CENTER_CENTER,
                "code map",
                FontId::proportional(14.0),
                ui.visuals().weak_text_color(),
            );
        }
        if let Some(l) = &state.legend {
            mapui::legend(ui, rect, l);
        }
    });
    state.canvas_rect = central.response.rect;
}

/// Mouse wheel (line/page units) zooms about the pointer; trackpad scrolling (point units)
/// pans; pinch and Cmd/Ctrl+scroll zoom; dragging pans; Home fits. Clicks, double clicks
/// (Shift held or not) and Escape are reported for the interaction (select, zoom to, path).
fn canvas_input(ui: &egui::Ui, response: &egui::Response, rect: Rect) -> CanvasInput {
    let mut input = CanvasInput::NONE;
    let rel = |p: egui::Pos2| [p.x - rect.min.x, p.y - rect.min.y];
    if response.dragged_by(PointerButton::Primary) || response.dragged_by(PointerButton::Middle) {
        let d = response.drag_delta();
        input.drag = [d.x, d.y];
    }
    input.hover = response.hover_pos().map(rel);
    input.click = response.clicked();
    input.double_click = response.double_clicked();
    input.shift = ui.input(|i| i.modifiers.shift);
    // Escape clears the selection unless a text field has the focus.
    input.escape = ui.memory(|m| m.focused().is_none()) && ui.input(|i| i.key_pressed(Key::Escape));
    if !response.hovered() {
        return input;
    }
    ui.input(|i| {
        let mut zoom = i.zoom_delta();
        for e in &i.events {
            if let Event::MouseWheel { unit, delta, modifiers, .. } = e {
                if modifiers.command || modifiers.ctrl {
                    continue; // already in zoom_delta
                }
                match unit {
                    MouseWheelUnit::Line => zoom *= 1.15f32.powf(delta.y),
                    MouseWheelUnit::Page => zoom *= 2f32.powf(delta.y),
                    MouseWheelUnit::Point => {
                        input.drag[0] += delta.x;
                        input.drag[1] += delta.y;
                    }
                }
            }
        }
        input.zoom = if zoom.is_finite() && zoom > 0.0 { zoom } else { 1.0 };
        input.zoom_anchor = i.pointer.hover_pos().map(rel);
        input.fit |= i.key_pressed(Key::Home);
    });
    input
}

fn left(ui: &mut egui::Ui, state: &mut UiState) {
    ui.add_space(4.0);
    panel::search(ui, &mut state.interact, &mut state.panel);
    ui.separator();
    ui.strong("Project");
    ui.weak(&state.project);
    ui.separator();
    egui::CollapsingHeader::new("Overlays").id_salt("pm_overlays").default_open(true).show(ui, |ui| {
        panel::overlays(ui, &mut state.interact);
    });
}

fn right(ui: &mut egui::Ui, state: &mut UiState) {
    ui.add_space(4.0);
    state.map_request_changed = false;
    if let Some(req) = state.map_request.as_mut() {
        ui.strong("Code map");
        state.map_request_changed = mapui::controls(ui, req);
        ui.separator();
    }
    ui.strong("Details");
    // Bar always shown: wrapped details would otherwise toggle it (and re-wrap) every frame.
    egui::ScrollArea::vertical()
        .id_salt("pm_details")
        .max_height(ui.available_height() * 0.8)
        .auto_shrink([false, true])
        .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysVisible)
        .show(ui, |ui| {
        if state.interact.selected.is_some() && !state.hover_text.is_empty() {
            ui.weak(&state.hover_text);
            ui.separator();
        }
        panel::details(ui, &mut state.interact, &mut state.panel, &state.hover_text);
    });
    ui.separator();
    if !state.map_info.is_empty() {
        ui.collapsing("Map", |ui| {
            ui.monospace(&state.map_info);
        });
    }
    ui.collapsing("Renderer", |ui| {
        ui.monospace(&state.renderer_info);
    });
}

fn bottom(ui: &mut egui::Ui, state: &mut UiState) {
    ui.add_space(4.0);
    panel::bottom(ui, &mut state.interact, &mut state.panel);
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
    fn code_map_controls_and_legend_stay_idle() {
        use crate::codemap::metrics::{ColorLegend, ColorMetric, EdgeMetric};
        let ctx = egui::Context::default();
        let mut s = state();
        s.map_request = Some(Request::default());
        for color_legend in [ColorLegend::Kinds, ColorLegend::Ramp { lo: "0", hi: "30+".to_owned() }] {
            s.legend = Some(Legend {
                size: pymapper_core::layout::SizeMetric::Loc,
                color: ColorMetric::Kind,
                color_legend,
                edge: EdgeMetric::Sites,
                edge_max: 12,
                edges: 3,
                segments: 9,
                rects: 10,
                has_muted: true,
            });
            let mut last = None;
            for _ in 0..4 {
                let raw = RawInput { screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(1280.0, 800.0))), ..RawInput::default() };
                let mut out = ctx.run_ui(raw, |ui| show(ui, &mut s));
                out.textures_delta.clear();
                last = Some(out.viewport_output[&ViewportId::ROOT].repaint_delay);
                assert!(!s.map_request_changed, "no input, no change");
            }
            assert_eq!(last, Some(std::time::Duration::MAX));
        }
        // A tiny canvas skips the legend instead of drawing outside it.
        s.map_status = "loading project: index".to_owned();
        layout(&ctx, &mut s, 300.0, 200.0, 1.0, 2);
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
