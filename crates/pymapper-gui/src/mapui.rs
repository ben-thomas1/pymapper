//! egui widgets of the code map: the metric controls (details panel) and the legend
//! (overlay at the canvas's bottom-left corner).

use egui::{Align2, Color32, CornerRadius, FontId, Pos2, Rect, Stroke, Vec2, pos2, vec2};

use crate::codemap::metrics::{self, ColorLegend, ColorMetric, EdgeMetric, KIND_COLORS, SIZE_METRICS};
use crate::codemap::scene::{CALL_HEAD, CALL_TAIL, EDGE_MAX_PT, EDGE_MIN_PT, IMPORT_HEAD, IMPORT_TAIL, Legend};
use crate::codemap::worker::Request;
use crate::map::scene::Rgba;
use pymapper_core::layout::Region;
use pymapper_core::resolve::Confidence;

/// Shows the controls for `req`; returns true if the user changed something.
pub fn controls(ui: &mut egui::Ui, req: &mut Request) -> bool {
    let mut changed = false;
    egui::Grid::new("pm_map_controls").num_columns(2).spacing([8.0, 4.0]).show(ui, |ui| {
        ui.label("Size");
        egui::ComboBox::from_id_salt("pm_size").selected_text(metrics::size_label(req.size)).show_ui(ui, |ui| {
            for m in SIZE_METRICS {
                changed |= ui.selectable_value(&mut req.size, m, metrics::size_label(m)).changed();
            }
        });
        ui.end_row();
        ui.label("Colour");
        egui::ComboBox::from_id_salt("pm_color").selected_text(req.scene.color.label()).show_ui(ui, |ui| {
            for m in ColorMetric::ALL {
                changed |= ui.selectable_value(&mut req.scene.color, m, m.label()).changed();
            }
        });
        ui.end_row();
        ui.label("Edges");
        egui::ComboBox::from_id_salt("pm_edge").selected_text(req.scene.edge.label()).show_ui(ui, |ui| {
            for m in EdgeMetric::ALL {
                changed |= ui.selectable_value(&mut req.scene.edge, m, m.label()).changed();
            }
        });
        ui.end_row();
        ui.label("Calls");
        egui::ComboBox::from_id_salt("pm_conf").selected_text(conf_label(req.scene.min_conf)).show_ui(ui, |ui| {
            for c in [Confidence::Unresolved, Confidence::Possible, Confidence::Likely, Confidence::Certain] {
                changed |= ui.selectable_value(&mut req.scene.min_conf, c as u8, conf_label(c as u8)).changed();
            }
        })
        .response
        .on_hover_text("Draw a call edge only if at least one call behind it is this sure");
        ui.end_row();
        ui.label("Bundling");
        let r = ui.add(egui::Slider::new(&mut req.scene.beta, 0.0..=1.0).fixed_decimals(2));
        // A drag reports every step: rebuild once it ends (clicks and typing commit at once).
        changed |= r.drag_stopped() || (r.changed() && !r.dragged());
        ui.end_row();
    });
    changed |= ui.checkbox(&mut req.scene.show_edges, "Show edges").changed();
    changed |= ui.checkbox(&mut req.scene.external_edges, "Edges inside third-party / stdlib").changed();
    ui.horizontal(|ui| {
        ui.label("Show");
        for (region, label) in [(Region::Main, "first-party"), (Region::ThirdParty, "third-party"), (Region::Stdlib, "stdlib")] {
            let bit = 1u8 << region as u8;
            let mut on = req.scene.origins & bit != 0;
            if ui.checkbox(&mut on, label).changed() {
                req.scene.origins ^= bit;
                changed = true;
            }
        }
    });
    changed
}

/// Label of a call confidence filter value.
pub fn conf_label(c: u8) -> &'static str {
    match Confidence::from_u8(c) {
        Confidence::Certain => "certain only",
        Confidence::Likely => "likely or surer",
        Confidence::Possible => "possible or surer",
        Confidence::Unresolved => "any",
    }
}

fn c32(c: Rgba) -> Color32 {
    let [r, g, b, a] = c.0;
    Color32::from_rgba_unmultiplied(r, g, b, a)
}

/// Draws the legend at the bottom-left of `canvas` (points).
pub fn legend(ui: &egui::Ui, canvas: Rect, l: &Legend) {
    let painter = ui.painter_at(canvas);
    let font = FontId::proportional(11.0);
    let text = ui.visuals().strong_text_color();
    let weak = ui.visuals().weak_text_color();
    let w = 230.0;
    let lines = 7.0 + if matches!(l.color_legend, ColorLegend::Kinds) { 1.0 } else { 0.0 } + if l.has_muted { 1.0 } else { 0.0 };
    let h = lines * 16.0 + 12.0;
    let min = pos2(canvas.min.x + 8.0, canvas.max.y - 8.0 - h);
    let frame = Rect::from_min_size(min, vec2(w, h));
    if !canvas.contains_rect(frame) {
        return; // canvas too small for the legend
    }
    painter.rect(frame, CornerRadius::same(4), Color32::from_black_alpha(190), Stroke::NONE, egui::StrokeKind::Inside);
    let x = min.x + 8.0;
    let mut y = min.y + 6.0;
    let row = |painter: &egui::Painter, s: &str, color: Color32, y: &mut f32| {
        painter.text(pos2(x, *y), Align2::LEFT_TOP, s, font.clone(), color);
        *y += 16.0;
    };
    row(&painter, &format!("size: {}", metrics::size_label(l.size)), text, &mut y);
    row(&painter, &format!("colour: {}", l.color.label()), text, &mut y);
    match &l.color_legend {
        ColorLegend::Kinds => {
            let kinds = ["dist", "pkg", "module", "class", "func"];
            let sw = (w - 16.0) / kinds.len() as f32;
            for (k, name) in kinds.iter().enumerate() {
                let p = pos2(x + k as f32 * sw, y);
                painter.rect_filled(Rect::from_min_size(p + vec2(0.0, 2.0), Vec2::splat(10.0)), 2.0, c32(KIND_COLORS[k]));
                painter.text(p + vec2(13.0, 0.0), Align2::LEFT_TOP, *name, font.clone(), weak);
            }
            y += 16.0;
            y += 16.0;
        }
        ColorLegend::Ramp { lo, hi } => {
            ramp_bar(&painter, Rect::from_min_size(pos2(x, y + 2.0), vec2(w - 16.0, 10.0)));
            y += 16.0;
            painter.text(pos2(x, y), Align2::LEFT_TOP, *lo, font.clone(), weak);
            painter.text(pos2(min.x + w - 8.0, y), Align2::RIGHT_TOP, hi, font.clone(), weak);
            y += 16.0;
        }
    }
    if l.has_muted {
        painter.rect_filled(Rect::from_min_size(pos2(x, y + 2.0), Vec2::splat(10.0)), 2.0, c32(metrics::muted(KIND_COLORS[2])));
        painter.text(pos2(x + 14.0, y), Align2::LEFT_TOP, "muted: third-party / stdlib", font.clone(), weak);
        y += 16.0;
    }
    row(&painter, &format!("edges: {} ({} shown)", l.edge.label(), l.edges), text, &mut y);
    // Width scale: thin = 1, thick = the largest weight drawn.
    let (tail, head) = if l.edge == EdgeMetric::Imports { (IMPORT_TAIL, IMPORT_HEAD) } else { (CALL_TAIL, CALL_HEAD) };
    let mid = y + 6.0;
    edge_sample(&painter, pos2(x, mid), 40.0, EDGE_MIN_PT, tail, head);
    painter.text(pos2(x + 46.0, y), Align2::LEFT_TOP, "1", font.clone(), weak);
    edge_sample(&painter, pos2(x + 80.0, mid), 40.0, EDGE_MAX_PT, tail, head);
    painter.text(pos2(x + 126.0, y), Align2::LEFT_TOP, format!("{} {}", l.edge_max, l.edge.unit()), font.clone(), weak);
    y += 16.0;
    let dir = if l.edge == EdgeMetric::Imports { "importer (blue) -> imported (violet), log width" } else { "caller (yellow) -> callee (pink), log width" };
    row(&painter, dir, weak, &mut y);
}

/// The sequential colour ramp as a horizontal bar.
fn ramp_bar(painter: &egui::Painter, r: Rect) {
    let n = 32;
    let step = r.width() / n as f32;
    for i in 0..n {
        let t = (i as f32 + 0.5) / n as f32;
        let cell = Rect::from_min_size(pos2(r.min.x + i as f32 * step, r.min.y), vec2(step + 0.5, r.height()));
        painter.rect_filled(cell, 0.0, c32(metrics::ramp(t)));
    }
}

/// A horizontal edge sample: tail colour on the left half, head colour on the right.
fn edge_sample(painter: &egui::Painter, left_mid: Pos2, len: f32, width: f32, tail: Rgba, head: Rgba) {
    let half = len * 0.5;
    let (a, b) = (c32(tail), c32(head));
    painter.line_segment([left_mid, left_mid + vec2(half, 0.0)], Stroke::new(width, a));
    painter.line_segment([left_mid + vec2(half, 0.0), left_mid + vec2(len, 0.0)], Stroke::new(width, b));
}

/// A status line over the canvas centre while the map loads (or why it failed).
pub fn status(ui: &egui::Ui, canvas: Rect, text: &str) {
    let painter = ui.painter_at(canvas);
    painter.text(canvas.center(), Align2::CENTER_CENTER, text, FontId::proportional(14.0), ui.visuals().weak_text_color());
}
