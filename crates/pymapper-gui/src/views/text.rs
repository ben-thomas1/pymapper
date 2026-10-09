//! Texts and selection outlines of the views controller: hover details, the selection /
//! status / summary lines of the panel, and the overlay marks of the selection.

use std::fmt::Write as _;
use std::sync::Arc;

use super::scene;
use super::ui::ViewsUi;
use super::worker::Content;
use super::{ViewKind, Views};
use crate::codemap::CodeMap;
use crate::map::MapState;
use crate::map::hit::{Hit, HitKind};
use crate::map::scene::RectInstance;

impl Views {
    /// Hover details of `h` in the active secondary view into `out` (cleared). Returns false
    /// in the code map (its own describer applies).
    pub fn describe(&self, h: Hit, out: &mut String) -> bool {
        out.clear();
        let Some(i) = self.active.slot() else { return false };
        let slot = &self.slots[i];
        let (Some(content), Some(cm)) = (slot.content.as_ref(), slot.map.as_ref()) else { return true };
        let ix = cm.analysis.index();
        match content {
            Content::NodeLink { graph, weight, calls, .. } => match h.kind {
                HitKind::Rect => {
                    if let Some(&v) = graph.nodes.get(h.id as usize) {
                        cm.describe_node(v, out);
                        if let Some(&hop) = graph.hops.get(h.id as usize) {
                            let _ = write!(out, "\n{hop} hop(s) from the focus");
                        }
                    }
                }
                HitKind::Line | HitKind::Curve => {
                    if let Some(e) = graph.edges.get(h.id as usize) {
                        let _ = write!(out, "{}\n-> {}", ix.qid_string(graph.nodes[e.src as usize]), ix.qid_string(graph.nodes[e.dst as usize]));
                        if graph.hops.is_empty() {
                            let _ = write!(out, "\ncall sites: {}\nimports: {}\n(edges: {})", e.sites, e.imports, weight.label());
                        } else {
                            let what = if *calls { "call sites" } else { "imports + call sites" };
                            let _ = write!(out, "\n{what}: {}", e.sites);
                        }
                    }
                }
            },
            Content::Matrix { dsm, .. } => {
                let (kind, idx) = scene::matrix_item(h.id);
                match kind {
                    scene::ID_CELL => {
                        if let Some(c) = dsm.cells.get(idx as usize) {
                            let (a, b) = (ix.qid_string(dsm.nodes[c.row as usize]), ix.qid_string(dsm.nodes[c.col as usize]));
                            let _ = write!(out, "{a}\ndepends on {b}\ncall sites: {}\nimports: {}", c.sites, c.imports);
                            if c.is_feedback() {
                                out.push_str("\nfeedback: part of a dependency cycle");
                            }
                        }
                    }
                    scene::ID_ROW | scene::ID_COL => {
                        if let Some(&v) = dsm.nodes.get(idx as usize) {
                            cm.describe_node(v, out);
                            let _ = write!(out, "\nrow {}", idx + 1);
                        }
                    }
                    scene::ID_BLOCK => {
                        if let Some(&(s, e)) = dsm.blocks.get(idx as usize) {
                            let _ = write!(out, "cycle: {} modules (rows {}..{})", e - s, s + 1, e);
                        }
                    }
                    _ => {}
                }
            }
        }
        true
    }

    /// Selection outlines of the active view into `out`: the code map cell, the node-link
    /// node, or the matrix row and column of the selection's deepest shown ancestor-or-self.
    pub(super) fn marks(&self, codemap: Option<&Arc<CodeMap>>, out: &mut Vec<RectInstance>) {
        out.clear();
        let Some(v) = self.selection else { return };
        let Some(i) = self.active.slot() else {
            out.extend(codemap.and_then(|cm| scene::codemap_mark(cm, v)));
            return;
        };
        let slot = &self.slots[i];
        let (Some(content), Some(cm)) = (slot.content.as_ref(), slot.map.as_ref()) else { return };
        let ix = cm.analysis.index();
        match content {
            Content::NodeLink { graph, nl, .. } => out.extend(graph.representative(ix, v).and_then(|l| nl.mark(l))),
            Content::Matrix { rows, geom, .. } => {
                // Binary search per ancestor: O(depth log n).
                let mut cur = (v < ix.node_count()).then_some(v);
                while let Some(c) = cur {
                    if let Ok(k) = rows.binary_search_by_key(&c, |r| r.0) {
                        out.extend_from_slice(&geom.marks(rows[k].1));
                        return;
                    }
                    cur = ix.parent(c);
                }
            }
        }
    }

    pub(super) fn update_marks(&self, map: &mut MapState, codemap: Option<&Arc<CodeMap>>) {
        let mut m = Vec::with_capacity(2);
        self.marks(codemap, &mut m);
        map.set_marks(&m);
    }

    /// Selection, status and summary texts (rewritten only when something changed).
    pub(super) fn refresh_texts(&self, ui: &mut ViewsUi, codemap: Option<&Arc<CodeMap>>) {
        ui.selection.clear();
        ui.status.clear();
        ui.info.clear();
        if let (Some(v), Some(cm)) = (self.selection, codemap)
            && v < cm.analysis.index().node_count()
        {
            ui.selection.push_str(&cm.analysis.index().qid_string(v));
        }
        let Some(i) = self.active.slot() else { return };
        if let Some(e) = &self.error {
            ui.status.push_str(e);
            return;
        }
        let slot = &self.slots[i];
        let pending = slot.key.is_some() && slot.scene_seq != slot.seq;
        match self.active {
            ViewKind::Focus if self.selection.is_none() => ui.status.push_str("Select a symbol (click a cell or node), then open Focus."),
            ViewKind::Graph if slot.key.is_some() && !slot.done => {
                let _ = write!(ui.status, "laying out: iteration {}", slot.iters);
            }
            _ if pending => ui.status.push_str("building the view"),
            _ => {}
        }
        let (Some(content), Some(cm)) = (slot.content.as_ref(), slot.map.as_ref()) else { return };
        let ms = slot.elapsed.as_secs_f64() * 1e3;
        match content {
            Content::NodeLink { graph, focus, calls, .. } => {
                let _ = write!(ui.info, "{} nodes, {} edges", graph.nodes.len(), graph.edges.len());
                if graph.dropped > 0 {
                    let _ = write!(ui.info, " ({} more left out)", graph.dropped);
                }
                match focus {
                    Some(f) => {
                        let what = if *calls { "call graph" } else { "module graph" };
                        let name = String::from_utf8_lossy(cm.analysis.index().name(graph.nodes[*f as usize]));
                        let _ = write!(ui.info, "\naround {name} ({what}), {ms:.1} ms");
                    }
                    None => {
                        let state = if slot.done { "converged" } else { "running" };
                        let _ = write!(ui.info, "\nBarnes-Hut layout: {} iterations, {ms:.0} ms, {state}", slot.iters);
                    }
                }
            }
            Content::Matrix { dsm, .. } => {
                let largest = dsm.blocks.iter().map(|b| b.1 - b.0).max().unwrap_or(0);
                let _ = write!(
                    ui.info,
                    "{} modules, {} marked cells\n{} cycles (largest {largest}), {} feedback cells, {ms:.1} ms\n\
                     Rows depend on columns. Blue below the diagonal: layered dependencies. \
                     Red above it: feedback (a cycle).",
                    dsm.nodes.len(),
                    dsm.cells.len(),
                    dsm.blocks.len(),
                    dsm.feedback
                );
            }
        }
    }
}
