//! Shared pieces of the llm mode of the query subcommands (`--llm`, design: ISSUES O4).
//!
//! ```text
//! # pymapper callers services.pricing:calculate_price          header: command and argument
//! target: services.pricing:calculate_price (function, services/pricing.py:6) · root: . · ...
//! Not call edges: functions passed as values (callbacks), ...  blind spots of the command
//!
//! ## app.py (app)                                              groups: path and module once
//! main [certain] at 31:11
//!
//! 3 more callers not displayed. Use --limit 0 to show all.     trailer (only when capped)
//! Next: `pymapper impact ...`                                   next step
//! ```
//!
//! No colour, no alignment padding; the same input gives the same bytes.

use pymapper_core::index::NodeId;

use super::render::Writer;
use super::session::Session;

/// `# pymapper <cmd> <args>` and a newline.
pub fn header(w: &mut Writer<'_>, cmd: &str, args: &str) {
    w.str("# pymapper ");
    w.str(cmd);
    if !args.is_empty() {
        w.str(" ");
        w.str(args);
    }
    w.end_line();
}

/// `root: <project root relative to the working directory>`.
pub fn root(w: &mut Writer<'_>, s: &Session) {
    w.str("root: ");
    w.path(&s.root);
}

/// `target: pkg.mod:f (function, pkg/mod.py:3)`.
pub fn target(w: &mut Writer<'_>, v: NodeId) {
    w.str("target: ");
    super::hops::describe_target(w, v);
}

/// ` · ` between header facts.
#[inline]
pub fn sep(w: &mut Writer<'_>) {
    w.str(" · ");
}

/// `n` followed by `one` or `many`.
pub fn count(w: &mut Writer<'_>, n: u64, one: &str, many: &str) {
    w.num(n);
    w.str(" ");
    w.str(if n == 1 { one } else { many });
}

/// What the call graph does not hold (callers, callees, impact, call paths and cycles), with
/// the share of call sites left unresolved in this program.
pub fn call_blind_spots(w: &mut Writer<'_>, s: &Session) {
    let st = s.calls.stats();
    w.str("Not call edges (results may be incomplete): functions passed as values (callbacks, registries), getattr/string dispatch, calls from C extensions and notebooks, and unresolved calls (");
    let tenths = (st.by_conf[3] * 1000 + st.calls / 2).checked_div(st.calls).unwrap_or(0);
    w.num(tenths / 10);
    w.str(".");
    w.num(tenths % 10);
    w.str("% of call sites).");
    w.end_line();
}

/// What the import graph does not hold (imports, import paths and cycles).
pub fn import_blind_spots(w: &mut Writer<'_>) {
    w.str("Not import edges: importlib.import_module / __import__ with computed names, imports in notebooks and C extensions.");
    w.end_line();
}

/// `## <path> (<module>)` for the group of `file` (`## (no file)` without one).
pub fn group(w: &mut Writer<'_>, file: Option<u32>) {
    w.end_line();
    w.str("## ");
    match file {
        Some(f) => {
            w.file(f);
            w.str(" (");
            let m = w.ix.file_node(f);
            w.qid(m);
            w.str(")");
        }
        None => w.str("(no file)"),
    }
    w.end_line();
}

/// The trailer of a capped list: `N more <what> not displayed. Use --limit 0 to show all.`,
/// and `N more outside PATHS not shown.` for rows the positional paths hid. Nothing when
/// neither applies.
pub fn trailer(w: &mut Writer<'_>, matched: usize, shown: usize, outside: usize, what: &str) {
    if shown < matched || outside > 0 {
        w.end_line();
    }
    if shown < matched {
        w.num((matched - shown) as u64);
        w.str(" more ");
        w.str(what);
        w.str(" not displayed. Use --limit 0 to show all.");
        w.end_line();
    }
    if outside > 0 {
        w.num(outside as u64);
        w.str(" more outside PATHS not shown (the whole project was analysed).");
        w.end_line();
    }
}

/// `Next: <text>` (after a blank line).
pub fn next(w: &mut Writer<'_>, text: &str) {
    w.end_line();
    w.str("Next: ");
    w.str(text);
    w.end_line();
}

/// A symbol as an argument of a suggested command: its qualified id, quoted when it holds
/// characters a shell would interpret (`#2` suffixes).
pub fn qid_arg(w: &Writer<'_>, v: NodeId) -> String {
    let id = w.ix.qid_string(v);
    if id.bytes().all(|b| b.is_ascii_alphanumeric() || b"._:-".contains(&b)) { id } else { format!("'{id}'") }
}

/// The project root as a shell argument, relative to the working directory (`.` there).
pub fn root_arg(s: &Session) -> String {
    use std::os::unix::ffi::OsStrExt;
    let b = s.cwd.as_os_str().as_bytes();
    let b = if b == b"/" { b } else { b.strip_suffix(b"/").unwrap_or(b) };
    let mut out = Vec::new();
    pymapper_core::output::push_relative(s.root.as_os_str().as_bytes(), b, &mut out);
    let text = String::from_utf8_lossy(&out).into_owned();
    if text.bytes().all(|c| c.is_ascii_alphanumeric() || b"._/-".contains(&c)) { text } else { format!("'{}'", text.replace('\'', "'\\''")) }
}

/// Row of class or function `v` in [`Index::metrics`](pymapper_core::index::Index::metrics):
/// metric rows follow the def rows without each file's leading module row (as
/// `hotspot::top_functions` reads them).
pub fn metric_row(ix: &pymapper_core::index::Index, v: NodeId) -> Option<usize> {
    if !ix.kind(v).is_def() {
        return None;
    }
    let (f, g) = (ix.file(v)?, ix.def_row(v)?);
    (g as u64).checked_sub(u64::from(f) + 1).map(|m| m as usize).filter(|&m| m < ix.metrics().cyclomatic.len())
}
