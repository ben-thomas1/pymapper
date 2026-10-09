//! egui side of the secondary views: the view tabs over the canvas's top-left corner, the
//! options of the active view (details panel), and the texts the controller fills in.

use egui::{Rect, vec2};

use super::graph::{Direction, Weight};
use super::{ViewKind, ViewOptions};

/// Per-window state of the view widgets (owned by [`crate::panels::UiState`]).
#[derive(Debug, Clone, Default)]
pub struct ViewsUi {
    /// Input: the view shown.
    pub active: ViewKind,
    /// Output: a tab was clicked this frame.
    pub tab: Option<ViewKind>,
    /// In/out: options of the views; the controls edit them in place.
    pub options: ViewOptions,
    /// Output: the user changed `options` this frame.
    pub options_changed: bool,
    /// Output: "focus here" was clicked (focus view on the selection).
    pub focus_selection: bool,
    /// Input: tabs are shown (a code map is loaded).
    pub enabled: bool,
    /// Input: qualified id of the selection (empty: none).
    pub selection: String,
    /// Input: status of the active secondary view over the canvas (empty: none).
    pub status: String,
    /// Input: summary of the active secondary view (details panel).
    pub info: String,
}

/// The view tabs at the top-left of `canvas` (points).
pub fn tabs(ui: &mut egui::Ui, canvas: Rect, s: &mut ViewsUi) {
    s.tab = None;
    if !s.enabled {
        return;
    }
    let bar = Rect::from_min_size(canvas.min + vec2(8.0, 6.0), vec2((canvas.width() - 16.0).max(0.0), 24.0));
    if !canvas.contains_rect(bar) {
        return;
    }
    ui.scope_builder(egui::UiBuilder::new().max_rect(bar), |ui| {
        ui.horizontal(|ui| {
            for k in ViewKind::ALL {
                let r = ui.selectable_label(s.active == k, k.label());
                if r.clicked() && s.active != k {
                    s.tab = Some(k);
                }
            }
        });
    });
}

/// Selection details and the options of the active view; sets the output flags.
pub fn controls(ui: &mut egui::Ui, s: &mut ViewsUi) {
    s.options_changed = false;
    s.focus_selection = false;
    if !s.enabled {
        return;
    }
    ui.strong("Selection");
    if s.selection.is_empty() {
        ui.weak("Click a cell, node or matrix row.");
    } else {
        ui.label(&s.selection);
        if s.active != ViewKind::Focus && ui.small_button("Focus view on it").clicked() {
            s.focus_selection = true;
        }
    }
    ui.separator();
    let o = &mut s.options;
    let mut changed = false;
    match s.active {
        ViewKind::CodeMap => return,
        ViewKind::Graph => {
            ui.strong("Graph (current level)");
            egui::Grid::new("pm_graph_opts").num_columns(2).spacing([8.0, 4.0]).show(ui, |ui| {
                ui.label("Edges");
                changed |= weight_combo(ui, "pm_graph_w", &mut o.graph.weight);
                ui.end_row();
                ui.label("Max nodes");
                let r = ui.add(egui::DragValue::new(&mut o.graph.max_nodes).range(10..=50_000).speed(10.0));
                changed |= r.drag_stopped() || (r.changed() && !r.dragged());
                ui.end_row();
                ui.label("Accuracy");
                // Barnes-Hut opening angle: smaller is more exact and slower.
                let r = ui.add(egui::Slider::new(&mut o.graph.force.theta, 0.3..=1.5).fixed_decimals(2).text("theta"));
                changed |= r.drag_stopped() || (r.changed() && !r.dragged());
                ui.end_row();
            });
        }
        ViewKind::Matrix => {
            ui.strong("Dependency matrix");
            egui::Grid::new("pm_matrix_opts").num_columns(2).spacing([8.0, 4.0]).show(ui, |ui| {
                ui.label("Cells");
                changed |= weight_combo(ui, "pm_matrix_w", &mut o.matrix.weight);
                ui.end_row();
            });
            changed |= ui.checkbox(&mut o.matrix.external, "Third-party / stdlib modules").changed();
            changed |= ui.checkbox(&mut o.matrix.scope_to_selection, "Only modules under the selection").changed();
        }
        ViewKind::Focus => {
            ui.strong("N-hop focus");
            egui::Grid::new("pm_focus_opts").num_columns(2).spacing([8.0, 4.0]).show(ui, |ui| {
                ui.label("Hops");
                changed |= ui.add(egui::Slider::new(&mut o.focus.hops, 1..=6)).changed();
                ui.end_row();
                ui.label("Follow");
                egui::ComboBox::from_id_salt("pm_focus_dir").selected_text(o.focus.direction.label()).show_ui(ui, |ui| {
                    for d in Direction::ALL {
                        changed |= ui.selectable_value(&mut o.focus.direction, d, d.label()).changed();
                    }
                });
                ui.end_row();
                ui.label("Max nodes");
                let r = ui.add(egui::DragValue::new(&mut o.focus.max_nodes).range(2..=20_000).speed(5.0));
                changed |= r.drag_stopped() || (r.changed() && !r.dragged());
                ui.end_row();
            });
        }
    }
    s.options_changed = changed;
    if !s.info.is_empty() {
        ui.add_space(4.0);
        ui.label(&s.info);
    }
    ui.separator();
}

fn weight_combo(ui: &mut egui::Ui, id: &str, w: &mut Weight) -> bool {
    let mut changed = false;
    egui::ComboBox::from_id_salt(id).selected_text(w.label()).show_ui(ui, |ui| {
        for m in Weight::ALL {
            changed |= ui.selectable_value(w, m, m.label()).changed();
        }
    });
    changed
}
