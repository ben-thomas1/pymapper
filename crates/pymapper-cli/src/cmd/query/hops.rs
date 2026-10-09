//! Rows of a call graph walk (`callers`, `callees`, `impact`).
//!
//! Concise, one reached node per line, located at the call site:
//!
//! ```text
//! app/main.py:5:12: app.main:run [certain]                        callers
//! app/cli.py:9:5: app.cli:main [likely] +1 site depth 2 via app.main:run
//! app/main.py:5:12: app.util:helper [certain] def app/util.py:1:5  callees
//! ```
//!
//! A row is shown when the reached node's file lies under the positional paths (the caller for
//! `callers` / `impact`, the callee for `callees`); sites are located for shown rows only.

use pymapper_core::query::{Hop, Query, Walk};

use super::render::{SymbolLoc, Writer};
use super::session::Session;
use super::sites::{Pair, locate};

/// Which side a walk went.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Side {
    /// Reverse edges: reached nodes call the target.
    Callers,
    /// Forward edges: the target calls the reached nodes.
    Callees,
}

/// Counts of the rows of a walk.
#[derive(Clone, Copy, Default)]
pub struct RowCounts {
    /// Reached nodes under the positional paths.
    pub matched: usize,
    /// Rows written (`matched` capped by the limit).
    pub shown: usize,
    /// Reached nodes outside the positional paths.
    pub outside: usize,
}

/// Writes the rows of `walk` (JSON: array elements, the array opened by the caller). The
/// concise tag is the edge's confidence, or with `path_conf` the least sure edge on the path
/// back to the target (JSON has both: `confidence`, `path_confidence`).
pub fn write_rows(s: &Session, q: &Query<'_>, w: &mut Writer<'_>, walk: &Walk, side: Side, limit: usize, path_conf: bool) -> RowCounts {
    let ix = q.index();
    let mut counts = RowCounts::default();
    let mut shown: Vec<&Hop> = Vec::new();
    for h in &walk.hops {
        let inside = s.node_in_scope(h.node);
        if !inside {
            counts.outside += 1;
            continue;
        }
        counts.matched += 1;
        if shown.len() < limit {
            shown.push(h);
        }
    }
    counts.shown = shown.len();
    let pairs: Vec<Pair> = shown
        .iter()
        .map(|h| match side {
            Side::Callers => Pair { src: h.node, dst: h.via, dispatch: h.dispatch },
            Side::Callees => Pair { src: h.via, dst: h.node, dispatch: h.dispatch },
        })
        .collect();
    let sites = pymapper_core::timing::time("output.sites", || locate(q, &pairs));
    for (i, (h, st)) in shown.iter().zip(&sites).enumerate() {
        if w.json {
            w.item(i);
            w.str("{");
            w.symbol_fields(h.node, true);
            w.key_str("confidence", h.conf.as_str());
            w.key_str("path_confidence", h.worst.as_str());
            w.key_bool("dispatch", h.dispatch);
            w.key_num("depth", u64::from(h.depth));
            w.key("via", false);
            w.qid(h.via);
            match st.first {
                Some(site) => w.key_site("site", site.file, site.line, site.col),
                None => {
                    w.key("site", false);
                    w.str("null");
                }
            }
            w.key_num("sites", u64::from(st.count));
            w.str("}");
            continue;
        }
        match st.first {
            Some(site) => w.loc(site.file, site.line, site.col),
            None => w.node_loc(h.node),
        }
        w.qid(h.node);
        if !ix.kind(h.node).is_def() {
            w.str(" (module level)");
        }
        w.conf_tag(if path_conf { h.worst } else { h.conf }, h.dispatch);
        if st.count > 1 {
            w.str(" +");
            w.num(u64::from(st.count - 1));
            w.str(if st.count == 2 { " site" } else { " sites" });
        }
        if side == Side::Callees {
            let d = SymbolLoc::of(ix, h.node);
            if let Some(f) = d.file {
                w.str(" def ");
                w.file(f);
                if d.line > 0 {
                    w.str(":");
                    w.num(u64::from(d.line));
                    w.str(":");
                    w.num(u64::from(d.col));
                }
            }
        }
        if h.depth > 1 {
            w.str(" depth ");
            w.num(u64::from(h.depth));
            w.str(" via ");
            w.qid(h.via);
        }
        w.end_line();
    }
    counts
}

/// The concise description of a target: `pkg.mod:f (function, pkg/mod.py:3)`.
pub fn describe_target(w: &mut Writer<'_>, v: pymapper_core::index::NodeId) {
    w.qid(v);
    w.str(" (");
    w.str(w.ix.kind(v).as_str());
    let d = SymbolLoc::of(w.ix, v);
    if let Some(f) = d.file {
        w.str(", ");
        w.file(f);
        if d.line > 0 {
            w.str(":");
            w.num(u64::from(d.line));
        }
    }
    w.str(")");
}

/// The concise tail of a summary line about filtering and the limit:
/// `; 3 outside PATHS not shown; first 200 shown (--limit 0 for all)`.
pub fn filter_note(w: &mut Writer<'_>, c: &RowCounts) {
    if c.outside > 0 {
        w.str("; ");
        w.num(c.outside as u64);
        w.str(" outside PATHS not shown");
    }
    if c.shown < c.matched {
        w.str("; first ");
        w.num(c.shown as u64);
        w.str(" shown (--limit 0 for all)");
    }
}

/// The JSON fields of [`RowCounts`] (inside an open object, with leading commas).
pub fn json_counts(w: &mut Writer<'_>, c: &RowCounts) {
    w.key_num("shown", c.shown as u64);
    w.key_num("outside_paths", c.outside as u64);
    w.key_bool("truncated", c.shown < c.matched);
}
