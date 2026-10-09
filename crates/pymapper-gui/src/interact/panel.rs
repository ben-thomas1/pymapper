//! egui widgets of the interaction: search box and results, overlay toggles, the details
//! panel (metrics, callers / callees, findings, path), the findings list and the source pane.
//!
//! Lists are virtualised (`ScrollArea::show_rows`): only visible rows are laid out, so a
//! venv's 200 search hits or 100k findings cost the same per frame as ten. Strings shown per
//! row are rendered into reused buffers; the source pane is laid out once per source (a
//! cached galley), not per frame.

use std::fmt::Write as _;
use std::sync::Arc;

use egui::text::{LayoutJob, TextFormat};
use egui::{Color32, FontId, Galley, RichText};

use pymapper_core::index::{Index, NodeId, NodeKind};
use pymapper_core::output::IndexContext;
use pymapper_core::query::{PathGraph, Walk};
use pymapper_core::resolve::Confidence;
use pymapper_core::rules::Severity;

use super::findings::NO_NODE;
use super::marks::{self, MarksSpec};
use super::source::{Highlighted, Style};
use super::state::Interaction;
use crate::codemap::load::Analysis;
use crate::codemap::metrics::kind_label;
use crate::map::scene::Rgba;

/// Height of one list row in points.
const ROW: f32 = 18.0;
/// Callers / callees listed in the details panel.
const NEIGHBOURS_SHOWN: usize = 12;

/// Which pane the bottom panel shows.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum BottomTab {
    #[default]
    Findings,
    Source,
}

/// egui-side caches of the interaction panels.
#[derive(Default)]
pub struct PanelState {
    pub tab: BottomTab,
    /// Findings shown (indices into the insights), for (insights pointer, min confidence).
    view: Vec<u32>,
    view_key: Option<(usize, u8)>,
    /// Laid-out source: (source generation, pixels per point, dark mode) and its galley.
    galley: Option<((u64, u32, bool), Arc<Galley>)>,
    text: String,
    bytes: Vec<u8>,
}

impl PanelState {
    /// The findings listed by the last findings tab layout (indices into the insights).
    pub fn findings_view(&self) -> &[u32] {
        &self.view
    }
}

impl std::fmt::Debug for PanelState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PanelState").field("tab", &self.tab).finish_non_exhaustive()
    }
}

fn c32(c: Rgba) -> Color32 {
    let [r, g, b, a] = c.0;
    Color32::from_rgba_unmultiplied(r, g, b, a)
}

fn severity_color(s: Severity) -> Color32 {
    c32(marks::severity_color(s))
}

/// `name (kind)` of node `v` into `out` (cleared).
fn node_line(ix: &Index, v: NodeId, out: &mut String) {
    out.clear();
    out.push_str(&String::from_utf8_lossy(ix.name(v)));
    let _ = write!(out, "  {}", kind_label(ix.kind(v)));
}

/// The qualified id of `v` into `out` (cleared).
fn qid(ix: &Index, v: NodeId, bytes: &mut Vec<u8>, out: &mut String) {
    bytes.clear();
    ix.qid(v, bytes);
    out.clear();
    out.push_str(&String::from_utf8_lossy(bytes));
}

/// The search box and its results (left panel). Clicking a result (or Enter) selects it and
/// frames it on the map.
pub fn search(ui: &mut egui::Ui, it: &mut Interaction, ps: &mut PanelState) {
    let r = ui.add(egui::TextEdit::singleline(&mut it.search_text).hint_text("Search symbols (fuzzy; pkg.mod:Name)").desired_width(f32::INFINITY));
    if r.changed() {
        it.update_search();
    }
    let enter = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
    let Some(analysis) = it.ctx.as_ref().map(|c| Arc::clone(&c.map.analysis)) else {
        ui.weak("No project loaded.");
        return;
    };
    if it.query_ready.is_none() {
        ui.weak("Indexing names...");
    }
    let Some(res) = it.search.as_ref() else { return };
    if it.search_text.trim().is_empty() {
        return;
    }
    let ix = &analysis.index;
    ui.weak(format!("{} matches ({:.1} ms)", res.hits.len(), res.elapsed.as_secs_f64() * 1000.0));
    let mut clicked = enter.then(|| res.hits.first().map(|h| h.node)).flatten();
    let n = res.hits.len();
    egui::ScrollArea::vertical().id_salt("pm_search_hits").max_height(ui.available_height() * 0.6).auto_shrink([false, true]).show_rows(ui, ROW, n, |ui, rows| {
        for i in rows {
            let v = res.hits[i].node;
            node_line(ix, v, &mut ps.text);
            let sel = it.selected == Some(v);
            let resp = ui.selectable_label(sel, ps.text.as_str());
            let resp = resp.on_hover_ui(|ui| {
                let mut t = String::new();
                qid(ix, v, &mut Vec::new(), &mut t);
                ui.label(t);
            });
            if resp.clicked() {
                clicked = Some(v);
            }
        }
    });
    if let Some(v) = clicked {
        it.focus(v);
    }
}

/// Overlay and highlight toggles (left panel).
pub fn overlays(ui: &mut egui::Ui, it: &mut Interaction) {
    let mut m: MarksSpec = it.marks;
    ui.checkbox(&mut m.cycles, "Import cycles").on_hover_text("Modules in import cycles (red) and each cycle's witness path");
    ui.checkbox(&mut m.layers, "Contract violations").on_hover_text("Imports breaking an import contract (orange)");
    ui.checkbox(&mut m.badges, "Finding badges").on_hover_text("A dot per cell with findings, coloured by the worst severity");
    ui.label("Min finding confidence");
    // Each step recounts the badges on the worker; pending recounts coalesce.
    ui.add(egui::Slider::new(&mut m.min_conf, 0..=100));
    it.set_marks(m);
    match &it.insights {
        None if it.worker.is_some() => {
            ui.weak("Analysing findings...");
        }
        None => {}
        Some(ins) => {
            let st = it.marks_stats;
            ui.weak(format!(
                "{} findings ({}); {} cycles, {} violations, {} badges shown",
                ins.findings.len(),
                ins.passes.join(", "),
                st.cycles,
                st.violations,
                st.badges
            ));
        }
    }
    ui.separator();
    let mut hl = it.hover_highlight;
    let mut dim = it.dim;
    ui.checkbox(&mut hl, "Highlight neighbourhood").on_hover_text("Highlight the hovered cell's callers, callees and their edges");
    ui.checkbox(&mut dim, "Dim the rest");
    if hl != it.hover_highlight || dim != it.dim {
        it.hover_highlight = hl;
        it.dim = dim;
        it.invalidate_highlight();
    }
    ui.add(egui::Label::new(RichText::new("Click: select. Double-click: zoom to. Shift-click: path from the selection. Esc: clear.").weak()).wrap());
}

/// The details of the selected node, else the hovered item's text (right panel).
pub fn details(ui: &mut egui::Ui, it: &mut Interaction, ps: &mut PanelState, hover_text: &str) {
    let (Some(v), Some(ctx)) = (it.selected, it.ctx.clone()) else {
        if hover_text.is_empty() {
            ui.weak("Nothing selected. Click a cell or search.");
        } else {
            ui.label(hover_text);
        }
        return;
    };
    let a = &*ctx.map.analysis;
    let ix = &a.index;
    if v >= ix.node_count() {
        return;
    }
    ui.horizontal(|ui| {
        ui.label(RichText::new(&it.selected_qid).strong().monospace());
        if ui.small_button("x").on_hover_text("Clear the selection (Esc)").clicked() {
            it.select(None);
        }
    });
    if it.selected.is_none() {
        return;
    }
    metrics(ui, a, v, &mut ps.text);
    if ui.small_button("Zoom to").clicked() {
        it.focus(v);
    }
    // Findings on the node.
    if let Some(ins) = it.insights.clone() {
        let mut first = true;
        let ctxi = IndexContext::new(ix, &a.root, &a.config.contracts);
        for i in ins.on_node(v).take(20) {
            if first {
                ui.separator();
                ui.strong("Findings");
                first = false;
            }
            let f = &ins.findings.as_slice()[i as usize];
            ps.text.clear();
            let _ = ins.findings.write_message(f, &ctxi, &mut ps.text);
            ui.horizontal_wrapped(|ui| {
                ui.colored_label(severity_color(f.rule.def().severity), format!("PM{:03}", f.rule.code()));
                ui.label(format!("{} (line {}, {}%)", ps.text, f.loc.line, f.confidence));
            });
        }
    }
    ui.separator();
    neighbours(ui, it, ix, ps);
    ui.separator();
    path(ui, it, ix, ps);
}

/// Metrics of node `v`.
fn metrics(ui: &mut egui::Ui, a: &Analysis, v: NodeId, buf: &mut String) {
    let ix = &a.index;
    let agg = a.agg.get(v);
    let kind = ix.kind(v);
    egui::Grid::new("pm_details_metrics").num_columns(2).spacing([12.0, 2.0]).show(ui, |ui| {
        let mut row = |k: &str, val: &dyn std::fmt::Display| {
            ui.weak(k);
            buf.clear();
            let _ = write!(buf, "{val}");
            ui.label(buf.as_str());
            ui.end_row();
        };
        row("kind", &kind_label(kind));
        if let Some(f) = ix.file(v) {
            row("origin", &ix.file_origin(f).as_str());
            let line = match (kind.is_def(), ix.def_row(v)) {
                (true, Some(r)) => ix.facts().defs.name_line[r],
                _ => 1,
            };
            let p = ix.file_path(f);
            let shown = p.strip_prefix(&a.root).unwrap_or(p);
            row("file", &format_args!("{}:{line}", shown.display()));
        }
        row("lines", &agg.loc);
        if kind != NodeKind::Function {
            row("symbols", &agg.symbols);
        }
        row("cyclomatic", &format_args!("sum {}  max {}", agg.cc_sum, agg.cc_max));
        row("cognitive", &format_args!("sum {}  max {}", agg.cog_sum, agg.cog_max));
        if let Some(r) = ix.metrics_row(v) {
            let m = ix.metrics().row(r);
            row("nesting", &m.max_nesting);
            row("halstead volume", &format_args!("{:.0}", m.volume));
            row("maintainability", &format_args!("{:.0} / 100", m.mi));
            if kind == NodeKind::Function {
                row("parameters", &ix.metrics().params[r]);
            }
            row("doc lines", &ix.metrics().doc_lines[r]);
        }
    });
}

/// Direct callers and callees of the selection (clickable).
fn neighbours(ui: &mut egui::Ui, it: &mut Interaction, ix: &Index, ps: &mut PanelState) {
    let Some(n) = it.neighbours.as_ref() else {
        ui.weak("Loading callers and callees...");
        return;
    };
    let mut clicked = None;
    let mut list = |ui: &mut egui::Ui, title: &str, w: &Walk, ps: &mut PanelState| {
        let more = if w.truncated { "+" } else { "" };
        ui.strong(format!("{title} ({}{more})", w.hops.len()));
        for h in w.hops.iter().take(NEIGHBOURS_SHOWN) {
            qid(ix, h.node, &mut ps.bytes, &mut ps.text);
            let conf = match h.conf {
                Confidence::Certain => "",
                Confidence::Likely => "  (likely)",
                Confidence::Possible => "  (possible)",
                Confidence::Unresolved => "  (unresolved)",
            };
            ps.text.push_str(conf);
            if h.dispatch {
                ps.text.push_str("  (override)");
            }
            if ui.link(ps.text.as_str()).clicked() {
                clicked = Some(h.node);
            }
        }
        if w.hops.len() > NEIGHBOURS_SHOWN {
            ui.weak(format!("... {} more", w.hops.len() - NEIGHBOURS_SHOWN));
        }
    };
    list(ui, "Callers", &n.callers, ps);
    list(ui, "Callees", &n.callees, ps);
    if let Some(v) = clicked {
        it.focus(v);
    }
}

/// Path controls and result.
fn path(ui: &mut egui::Ui, it: &mut Interaction, ix: &Index, ps: &mut PanelState) {
    ui.horizontal(|ui| {
        ui.strong("Path");
        let mut g = it.path_graph;
        ui.selectable_value(&mut g, PathGraph::Calls, "calls");
        ui.selectable_value(&mut g, PathGraph::Imports, "imports");
        if g != it.path_graph {
            it.path_graph = g;
            // Re-run the current path on the other graph.
            if let Some(p) = it.path.as_ref().map(|p| p.job) {
                it.request_path(p.from, p.to);
            }
        }
        if it.path.is_some() && ui.small_button("clear").clicked() {
            it.clear_path();
        }
    });
    if it.path_pending() {
        ui.weak("Searching...");
        return;
    }
    let Some(p) = it.path.as_ref() else {
        ui.weak("Shift-click a cell (or a search result) to find the shortest path from the selection to it.");
        return;
    };
    let Some(sp) = p.path.as_ref() else {
        qid(ix, p.job.to, &mut ps.bytes, &mut ps.text);
        ui.weak(format!("No {} path to {}.", if p.job.graph == PathGraph::Calls { "call" } else { "import" }, ps.text));
        return;
    };
    ui.weak(format!("{} hops ({:.2} ms)", sp.len(), p.elapsed.as_secs_f64() * 1000.0));
    let mut clicked = None;
    for (i, &v) in sp.nodes.iter().enumerate() {
        qid(ix, v, &mut ps.bytes, &mut ps.text);
        if let Some(c) = (i > 0).then(|| sp.conf(i - 1)).flatten()
            && c != Confidence::Certain
        {
            let _ = write!(ps.text, "  ({c:?})");
        }
        if ui.link(format!("{}. {}", i + 1, ps.text)).clicked() {
            clicked = Some(v);
        }
    }
    if let Some(v) = clicked {
        // Keep the path: frame the node without re-selecting.
        it.focus_request = it.cell_rect(v);
    }
}

/// The bottom panel: findings list or source pane.
pub fn bottom(ui: &mut egui::Ui, it: &mut Interaction, ps: &mut PanelState) {
    ui.horizontal(|ui| {
        ui.selectable_value(&mut ps.tab, BottomTab::Findings, RichText::new("Findings").strong());
        ui.selectable_value(&mut ps.tab, BottomTab::Source, RichText::new("Source").strong());
        if ps.tab == BottomTab::Source
            && let Some(s) = &it.source
        {
            let p = it.ctx.as_ref().map(|c| c.map.analysis.root.as_path());
            let shown = p.and_then(|r| s.path.strip_prefix(r).ok()).unwrap_or(&s.path);
            ui.weak(format!("{}  ({:.2} ms)", shown.display(), s.elapsed.as_secs_f64() * 1000.0));
        }
    });
    match ps.tab {
        BottomTab::Findings => findings_list(ui, it, ps),
        BottomTab::Source => source_pane(ui, it, ps),
    }
    // Keep the panel's height when the pane has little to show.
    let rest = ui.available_rect_before_wrap();
    if rest.is_positive() {
        ui.allocate_rect(rest, egui::Sense::hover());
    }
}

/// Every finding at or above the confidence threshold, worst first. Click: select and frame.
fn findings_list(ui: &mut egui::Ui, it: &mut Interaction, ps: &mut PanelState) {
    let (Some(ins), Some(ctx)) = (it.insights.clone(), it.ctx.clone()) else {
        ui.weak(if it.worker.is_some() { "Analysing findings..." } else { "No findings." });
        return;
    };
    let min_conf = it.marks.min_conf;
    let key = (Arc::as_ptr(&ins) as usize, min_conf);
    if ps.view_key != Some(key) {
        ps.view.clear();
        ps.view.extend(ins.findings.as_slice().iter().enumerate().filter(|(_, f)| f.confidence >= min_conf).map(|(i, _)| i as u32));
        ps.view_key = Some(key);
    }
    if ps.view.is_empty() {
        ui.weak("No findings at this confidence.");
        return;
    }
    let a = &*ctx.map.analysis;
    let ix = &a.index;
    let ctxi = IndexContext::new(ix, &a.root, &a.config.contracts);
    let mut clicked = None;
    let view = std::mem::take(&mut ps.view);
    // Vertical only, rows truncated to the panel width: a horizontal bar would change the
    // visible row count and the widest row with it (layout oscillation, endless repaints).
    egui::ScrollArea::vertical().id_salt("pm_findings").auto_shrink([false, false]).show_rows(ui, ROW, view.len(), |ui, rows| {
        for k in rows {
            let i = view[k] as usize;
            let f = &ins.findings.as_slice()[i];
            ps.text.clear();
            let _ = write!(ps.text, "PM{:03}  ", f.rule.code());
            if f.loc.file.0 < ix.file_count() {
                let p = ix.file_path(f.loc.file.0);
                let shown = p.strip_prefix(&a.root).unwrap_or(p);
                let _ = write!(ps.text, "{}:{}  ", shown.display(), f.loc.line);
            }
            let _ = ins.findings.write_message(f, &ctxi, &mut ps.text);
            let _ = write!(ps.text, "  ({}%)", f.confidence);
            let sel = it.selected.is_some() && it.selected == ins.nodes.get(i).copied();
            let color = severity_color(f.rule.def().severity);
            let text = RichText::new(ps.text.as_str()).color(if sel { ui.visuals().strong_text_color() } else { color });
            let r = ui.add(egui::Label::new(text).truncate().sense(egui::Sense::click()));
            if r.clicked() && ins.nodes[i] != NO_NODE {
                clicked = Some(ins.nodes[i]);
            }
        }
    });
    ps.view = view;
    if let Some(v) = clicked {
        it.focus(v);
    }
}

/// Colour of a source style (dark or light theme).
fn style_color(s: Style, dark: bool) -> Color32 {
    let (d, l) = match s {
        Style::Plain => ((220, 223, 228), (30, 32, 36)),
        Style::Keyword => ((198, 120, 221), (152, 24, 160)),
        Style::DefName => ((97, 175, 239), (20, 90, 190)),
        Style::Builtin => ((86, 182, 194), (10, 120, 130)),
        Style::Decorator => ((229, 192, 123), (150, 100, 0)),
        Style::String => ((152, 195, 121), (40, 125, 30)),
        Style::Number => ((209, 154, 102), (170, 80, 0)),
        Style::Comment => ((127, 132, 142), (120, 124, 130)),
        Style::Operator => ((171, 178, 191), (70, 74, 80)),
        Style::Error => ((240, 80, 80), (200, 20, 20)),
    };
    let c = if dark { d } else { l };
    Color32::from_rgb(c.0, c.1, c.2)
}

/// A layout job of `h` with a line-number gutter.
pub fn source_job(h: &Highlighted, dark: bool, font: FontId) -> LayoutJob {
    let mut job = LayoutJob::default();
    let gutter = TextFormat { font_id: font.clone(), color: style_color(Style::Comment, dark), ..TextFormat::default() };
    let last = h.first_line + h.lines().max(1) - 1;
    let width = last.to_string().len();
    let mut line = h.first_line;
    let mut num = String::new();
    let mut push_num = |job: &mut LayoutJob, line: u32| {
        num.clear();
        let _ = write!(num, "{line:>width$}  ");
        job.append(&num, 0.0, gutter.clone());
    };
    push_num(&mut job, line);
    for r in &h.runs {
        let fmt = TextFormat { font_id: font.clone(), color: style_color(r.style, dark), ..TextFormat::default() };
        let text = &h.text[r.start as usize..r.end as usize];
        let mut rest = text;
        while let Some(i) = rest.find('\n') {
            job.append(&rest[..=i], 0.0, fmt.clone());
            rest = &rest[i + 1..];
            line += 1;
            if line <= last {
                push_num(&mut job, line);
            }
        }
        if !rest.is_empty() {
            job.append(rest, 0.0, fmt);
        }
    }
    if h.truncated {
        job.append("\n... (cut: only the first 256 KiB of the span are read)", 0.0, gutter);
    }
    job
}

/// The selected node's source, re-read from disk and coloured by the core lexer.
fn source_pane(ui: &mut egui::Ui, it: &mut Interaction, ps: &mut PanelState) {
    let Some(v) = it.selected else {
        ui.weak("Select a symbol to see its source.");
        return;
    };
    let Some(s) = it.source.as_ref().filter(|s| s.node == v) else {
        ui.weak("Reading...");
        return;
    };
    let h = match &s.source {
        Ok(h) => h,
        Err(e) => {
            ui.weak(format!("No source: {e}"));
            return;
        }
    };
    let dark = ui.visuals().dark_mode;
    let key = (it.source_gen, ui.ctx().pixels_per_point().to_bits(), dark);
    let galley = match &ps.galley {
        Some((k, g)) if *k == key => Arc::clone(g),
        _ => {
            let job = source_job(h, dark, FontId::monospace(12.0));
            let g = ui.ctx().fonts_mut(|f| f.layout_job(job));
            ps.galley = Some((key, Arc::clone(&g)));
            g
        }
    };
    egui::ScrollArea::both().id_salt("pm_source").auto_shrink([false, false]).show(ui, |ui| {
        ui.add(egui::Label::new(galley).selectable(true));
    });
}
