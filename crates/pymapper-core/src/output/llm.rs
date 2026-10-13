//! The `llm` format (agent mode; design: ISSUES O2): a compact, prioritised report in a
//! Markdown-like layout, capped at [`Options::limit`] items with an "N more findings not
//! displayed" trailer, stable ids, evidence (confidence, reasons, metric values), a fix hint and
//! a verification command per item, and header facts (root, mode, counts, what the exit code
//! means, blind spots). Never coloured; deterministic.
//!
//! ```text
//! # pymapper check · root /abs/demo (reached as .) · paths relative to the working directory
//! Dead-code mode: application ([project.scripts] in pyproject.toml)
//! 26 findings in 15 files, 17 items, showing 3 · 10 below confidence 80 not shown (...) · exit 1 = findings, not an error
//! Not analysed: notebooks, C extensions, names used only via getattr or strings. Verify dead code before deleting it.
//! Rules not run: PM501: needs `analysis.dupes = true`
//!
//! ## Architecture (2)
//! PM301-a1f3 repo/order_repo.py:1 import cycle (2 modules): repo.order_repo -> models.order -> repo.order_repo
//!   fix: break one import of the cycle: ... · verify: pymapper explain PM301-a1f3
//! ...
//! 14 more findings not displayed (in 9 files). Use --limit 0 to show all, or --select <code> to narrow.
//! Next: pymapper explain <id> for evidence and source · suppress with # pymapper: ignore[PMxxx] on the reported line
//! ```
//!
//! # Items and priority
//!
//! An item is one decision (see [`group`]): metric findings of one symbol, a dead definition
//! with everything only it uses ("also dead if it goes"), dead definitions of one file with the
//! same verdict, the unused imports of a file, a cycle, a contract break, one duplicate pair
//! (PM501 is reported in both directions; shown once), a dependency with all its import sites.
//! A merged item shows the id of its first finding (a chain: its root). Sections follow the
//! human format (O1): Architecture, Dependencies, Complexity, Coupling, Duplication, Dead code,
//! Unused imports. Within a section: severity, then confidence, then (complexity, coupling) the
//! worst metric overshoot, (dead code) chains and modules before single symbols, then path
//! order. The cap counts items.
//!
//! Paths are relative to the working directory (as in the other text formats); symbols are
//! shown without their module (the path is on the line). The header facts the findings do not
//! carry (root as reached, dead-code modes, skipped rules, path filter) come from
//! [`RunNotes`].

mod group;
mod item;
#[cfg(test)]
mod tests;

use self::group::{COMPLEXITY, DEAD, DEPS, DUPLICATION, Groups, IMPORTS, SECTIONS};
use super::buf::Out;
use super::{Options, Report, RunNotes, Summary};
use crate::rules::codes::LONG_MODULE;
use crate::rules::{FileId, Finding};

/// Blind spots of `check`, stated once in the header.
pub const CHECK_BLIND_SPOTS: &str =
    "Not analysed: notebooks, C extensions, names used only via getattr or strings. Verify dead code before deleting it.";
/// Blind spots of `deps --check`.
pub const DEPS_BLIND_SPOTS: &str = "Not analysed: imports in notebooks and imports by name (importlib, __import__, plugins, \
entry points). Verify before removing a dependency.";
/// The last line of a report with findings.
pub const NEXT: &str =
    "Next: pymapper explain <id> for evidence and source · suppress with # pymapper: ignore[PMxxx] on the reported line\n";

pub(super) fn write(r: &Report<'_>, opts: &Options, summary: &Summary<'_>, out: &mut Out<'_>) {
    let fs: Vec<&Finding> = r.iter().collect();
    let ids = r.ids();
    let g = group::build(r, &fs);
    let limit = if opts.limit == 0 { usize::MAX } else { opts.limit as usize };
    let shown = g.items.len().min(limit);
    let notes = &summary.notes;
    header(r, &g, opts, summary, shown, out);
    if g.items.is_empty() {
        return;
    }
    // Per section: items, findings, items shown; and complexity items about modules.
    let mut counts = [(0u32, 0u32, 0u32); SECTIONS.len()];
    let mut modules = [0u32; SECTIONS.len()];
    for (k, item) in g.items.iter().enumerate() {
        let c = &mut counts[item.section as usize];
        c.0 += 1;
        c.1 += item.len;
        if k < shown {
            c.2 += 1;
        }
        if fs[g.of(item)[0] as usize].rule == LONG_MODULE {
            modules[item.section as usize] += 1;
        }
    }
    let mut w = item::Writer::new(r, &fs, &ids, &g, if notes.root.is_empty() { "." } else { notes.root });
    let mut current = u8::MAX;
    for item in &g.items[..shown] {
        if item.section != current {
            current = item.section;
            section_header(current, counts[current as usize], modules[current as usize], out);
        }
        w.item(item, out);
    }
    out.byte(b'\n');
    if shown < g.items.len() {
        let mut hidden = 0u64;
        let mut files: Vec<u32> = Vec::new();
        for item in &g.items[shown..] {
            hidden += u64::from(item.len);
            for &pos in g.of(item) {
                let file = fs[pos as usize].loc.file;
                if file != FileId::NONE {
                    files.push(file.0);
                }
            }
        }
        files.sort_unstable();
        files.dedup();
        out.u32(hidden.min(u64::from(u32::MAX)) as u32);
        out.str(if hidden == 1 { " more finding not displayed (in " } else { " more findings not displayed (in " });
        out.u32(files.len() as u32);
        out.str(if files.len() == 1 { " file" } else { " files" });
        out.str("). Use --limit 0 to show all, or --select <code> to narrow.\n");
    }
    out.str(NEXT);
}

/// The header: command and root, dead-code modes, filter, counts and exit meaning, blind spots,
/// rules not run.
fn header(r: &Report<'_>, g: &Groups, opts: &Options, summary: &Summary<'_>, shown: usize, out: &mut Out<'_>) {
    let notes: &RunNotes<'_> = &summary.notes;
    let command = if notes.command.is_empty() { "check" } else { notes.command };
    out.str("# pymapper ");
    out.str(command);
    let mut root = Vec::new();
    r.ctx().write_root_dir(&mut root);
    if !root.is_empty() {
        out.str(" · root ");
        super::lossy_tail(&mut root, 0);
        out.bytes(&root);
        if !notes.root.is_empty() {
            out.str(" (reached as ");
            out.str(notes.root);
            out.byte(b')');
        }
    }
    out.str(" · paths relative to the working directory\n");
    match notes.modes {
        [] => {}
        [one] => {
            out.str("Dead-code mode: ");
            out.str(one);
            out.byte(b'\n');
        }
        many => {
            out.str("Dead-code mode per unit:\n");
            for m in many {
                out.str("- ");
                out.str(m);
                out.byte(b'\n');
            }
        }
    }
    if !notes.filter.is_empty() {
        out.str("Filtered to ");
        out.str(notes.filter);
        out.str("; the whole project was analysed.\n");
    }
    let n = r.len();
    let gated = r.gated_count(opts.min_confidence);
    if n == 0 {
        out.str("No findings");
    } else {
        out.u32(n as u32);
        out.str(if n == 1 { " finding in " } else { " findings in " });
        let files = r.file_count();
        out.u32(files as u32);
        out.str(if files == 1 { " file, " } else { " files, " });
        let items = g.items.len();
        out.u32(items as u32);
        out.str(if items == 1 { " item" } else { " items" });
        if shown < items {
            out.str(", showing ");
            out.u32(shown as u32);
        } else {
            out.str(", all shown");
        }
    }
    let below_shown = (n - gated) as u32;
    let hidden_below = summary.filter.below_confidence.saturating_sub(below_shown);
    if hidden_below > 0 {
        out.str(" · ");
        out.u32(hidden_below);
        out.str(" below confidence ");
        out.u32(u32::from(opts.min_confidence));
        out.str(" not shown (--min-confidence 0 shows them)");
    }
    for (k, what) in [
        (summary.filter.suppressed, " suppressed"),
        (r.baselined(), " baselined"),
        (summary.filter.file_ignored, " ignored by per-file-ignores"),
    ] {
        if k > 0 {
            out.str(" · ");
            out.u32(k);
            out.str(what);
        }
    }
    out.str(if gated > 0 { " · exit 1 = findings, not an error\n" } else { " · exit 0 = clean\n" });
    out.str(if command.starts_with("deps") { DEPS_BLIND_SPOTS } else { CHECK_BLIND_SPOTS });
    out.byte(b'\n');
    if !notes.skipped.is_empty() {
        out.str("Rules not run: ");
        for (i, s) in notes.skipped.iter().enumerate() {
            if i > 0 {
                out.str("; ");
            }
            out.str(s);
        }
        out.byte(b'\n');
    }
}

/// `## Name (counts)`: items (functions, pairs, ...), findings when merged, and the items shown
/// when the cap cut the section.
fn section_header(section: u8, (items, findings, shown): (u32, u32, u32), modules: u32, out: &mut Out<'_>) {
    let plural = |n: u32, one: &'static str, many: &'static str| if n == 1 { one } else { many };
    out.str("\n## ");
    out.str(SECTIONS[section as usize]);
    out.str(" (");
    match section {
        COMPLEXITY => {
            let functions = items - modules;
            if functions > 0 {
                out.u32(functions);
                out.str(plural(functions, " function", " functions"));
            }
            if modules > 0 {
                out.str(if functions > 0 { ", " } else { "" });
                out.u32(modules);
                out.str(plural(modules, " module", " modules"));
            }
        }
        DUPLICATION => {
            out.u32(items);
            out.str(plural(items, " pair", " pairs"));
        }
        DEAD if findings != items => {
            out.u32(items);
            out.str(" items, ");
            out.u32(findings);
            out.str(" symbols");
        }
        IMPORTS | DEPS if findings != items => {
            out.u32(findings);
            out.str(" in ");
            out.u32(items);
            out.str(plural(items, " item", " items"));
        }
        _ => out.u32(items),
    }
    if shown < items {
        out.str("; ");
        out.u32(shown);
        out.str(" shown");
    }
    out.str(")\n");
}
