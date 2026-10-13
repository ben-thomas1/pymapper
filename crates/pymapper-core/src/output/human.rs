//! The `human` format (default; design: ISSUES O1): findings grouped by category, then file
//! (or by file with `group-by = "file"`), metric findings merged per symbol, cross-file findings
//! shown once, short symbols, a stable id on every item, a per-category footer tally, ANSI
//! colour when [`Options::color`] is set. Never capped, never truncated.
//!
//! ```text
//! Architecture  1
//!   models/order.py
//!      2:29  imports forbidden repo.order_repo (contract: models are independent of repo)  PM303-77c0
//!
//! Complexity  1 function
//!   services/pricing.py
//!      6:5   calculate_price  McCabe 14>10 · cognitive 32>15                 PM101-0b2e +PM102
//!
//! Duplication  1 pair
//!   services/invoice.py:1  invoice_lines  ↔  services/pricing.py:39  receipt_lines  (12 lines)  PM501-c912
//!
//! 5 findings in 3 files (23 files checked) · architecture 1 · complexity 1 function · duplication 1 pair
//! ```
//!
//! - **Sections** in priority order ([`Section`]): architecture (PM3xx), dependencies (PM6xx),
//!   complexity (PM1xx), coupling (PM2xx), duplication (PM5xx), dead code (PM401-404), unused
//!   imports (PM405). Files alphabetically (the report's path order), findings by line.
//! - **Merging:** PM101-107 findings on the same symbol and place form one line, built from
//!   the structured value/threshold arguments; so do PM201-204 (same shape). The line ends
//!   with the first finding's id, then the other codes (`+PM102,PM105`).
//! - **Once:** an import cycle is one finding, under the file holding the import that closes
//!   it; a contract break is under the importing file; of the two PM501 findings of a pair
//!   (one per copy) only the first in output order is shown, with both locations.
//! - **Ids** end every line, right-aligned to the line width: `COLUMNS` when set (clamped to
//!   [`MIN_WIDTH`]..=[`MAX_WIDTH`]), else [`DEFAULT_WIDTH`]. A line too long for that gets the
//!   id two spaces after its text (nothing is ever cut). There is deliberately no terminal
//!   size query: the layout is the same on a terminal, in a pipe and in a test.
//! - **Footer:** counts and the per-section tally, then the notes on what is not shown. The
//!   CLI adds its own lines after it (path filter, skipped rules).

use super::buf::Out;
use super::style;
use super::{GroupBy, Options, Report, Summary};
use crate::rules::codes::{DUPLICATE_CODE, UNUSED_IMPORT};
use crate::rules::{Arg, FileId, Finding, Rule, SymbolId};

mod body;
mod render;
#[cfg(test)]
mod tests;

use render::{Writer, footer};

/// Line width the ids are aligned to when `COLUMNS` is not set.
pub const DEFAULT_WIDTH: usize = 100;
/// Smallest `COLUMNS` honoured (smaller values fall back to [`DEFAULT_WIDTH`]).
pub const MIN_WIDTH: usize = 40;
/// Widest alignment: on a wide terminal, ids further right are hard to match with their line.
pub const MAX_WIDTH: usize = 120;

pub(super) fn write(r: &Report<'_>, opts: &Options, summary: &Summary, out: &mut Out<'_>) {
    write_with_width(r, opts, summary, width_from_env(), out);
}

/// The id alignment width: `COLUMNS` if set and sensible, else [`DEFAULT_WIDTH`].
fn width_from_env() -> usize {
    match std::env::var("COLUMNS").ok().and_then(|v| v.trim().parse::<usize>().ok()) {
        Some(w) if w >= MIN_WIDTH => w.min(MAX_WIDTH),
        _ => DEFAULT_WIDTH,
    }
}

/// [`write`] with an explicit alignment width.
fn write_with_width(r: &Report<'_>, opts: &Options, summary: &Summary, width: usize, out: &mut Out<'_>) {
    let fs: Vec<&Finding> = r.iter().collect();
    let mut items = items(r, &fs);
    let mut w = Writer::new(r, &fs, opts.color, width);
    match opts.group_by {
        GroupBy::Category => {
            // Stable: within a section, items keep the report's (path, line) order.
            items.sort_by_key(|it| it.section);
            w.by_category(&items, out);
        }
        GroupBy::File => w.by_file(&items, out),
    }
    if opts.footer {
        if !items.is_empty() {
            out.byte(b'\n');
        }
        footer(r, opts, summary, &items, &fs, out);
    }
}

/// Report sections, in output order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
enum Section {
    Architecture,
    Dependencies,
    Complexity,
    Coupling,
    Duplication,
    DeadCode,
    UnusedImports,
    /// Codes outside the reserved ranges (none are registered today).
    Other,
}

impl Section {
    fn of(rule: Rule) -> Section {
        match rule.code() / 100 {
            3 => Section::Architecture,
            6 => Section::Dependencies,
            1 => Section::Complexity,
            2 => Section::Coupling,
            5 => Section::Duplication,
            4 if rule == UNUSED_IMPORT => Section::UnusedImports,
            4 => Section::DeadCode,
            _ => Section::Other,
        }
    }

    /// Section header.
    fn title(self) -> &'static str {
        match self {
            Section::Architecture => "Architecture",
            Section::Dependencies => "Dependencies",
            Section::Complexity => "Complexity",
            Section::Coupling => "Coupling",
            Section::Duplication => "Duplication",
            Section::DeadCode => "Dead code",
            Section::UnusedImports => "Unused imports",
            Section::Other => "Other",
        }
    }

    /// Name in the footer tally.
    fn tally(self) -> &'static str {
        match self {
            Section::Architecture => "architecture",
            Section::Dependencies => "dependencies",
            Section::Complexity => "complexity",
            Section::Coupling => "coupling",
            Section::Duplication => "duplication",
            Section::DeadCode => "dead code",
            Section::UnusedImports => "unused imports",
            Section::Other => "other",
        }
    }

    /// Category column of the file layout (padded to [`LABEL_WIDTH`]).
    fn label(self) -> &'static str {
        match self {
            Section::Architecture => "architecture",
            Section::Dependencies => "dependency",
            Section::Complexity => "complexity",
            Section::Coupling => "coupling",
            Section::Duplication => "duplication",
            Section::DeadCode => "dead code",
            Section::UnusedImports => "unused import",
            Section::Other => "other",
        }
    }

    fn color(self) -> &'static [u8] {
        match self {
            Section::Architecture => style::RED,
            Section::Dependencies => style::CYAN,
            Section::Complexity => style::MAGENTA,
            Section::Coupling => style::MAGENTA_DIM,
            Section::Duplication => style::BLUE,
            Section::DeadCode => style::YELLOW,
            Section::UnusedImports => style::YELLOW_DIM,
            Section::Other => style::BOLD,
        }
    }
}

/// Width of the longest [`Section::label`].
const LABEL_WIDTH: usize = 13;

/// Metric rules whose findings on one symbol merge into one line.
fn is_metric(rule: Rule) -> bool {
    matches!(rule.code(), 101..=107 | 201..=204)
}

/// One output line: the findings at report positions `pos..pos + len` (more than one only for
/// merged metrics).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Item {
    section: Section,
    pos: u32,
    len: u32,
}

/// The report's findings as output lines, in report order: metric findings merged, the second
/// finding of each duplicate pair dropped.
fn items(r: &Report<'_>, fs: &[&Finding]) -> Vec<Item> {
    let mirrors = duplicate_mirrors(r, fs);
    let mut items: Vec<Item> = Vec::with_capacity(fs.len());
    for (pos, f) in fs.iter().enumerate() {
        let pos = pos as u32;
        if mirrors.binary_search(&pos).is_ok() {
            continue;
        }
        let section = Section::of(f.rule);
        if is_metric(f.rule)
            && f.symbol != SymbolId::NONE
            && let Some(last) = items.last_mut()
            && last.section == section
            && last.pos + last.len == pos
        {
            let g = fs[last.pos as usize];
            if is_metric(g.rule) && g.symbol == f.symbol && g.loc == f.loc {
                last.len += 1;
                continue;
            }
        }
        items.push(Item { section, pos, len: 1 });
    }
    items
}

/// The other copy of a PM501 pair, as `(file, line, end_line, symbol)` from its extra args.
fn other_copy(r: &Report<'_>, f: &Finding) -> Option<(FileId, u32, u32, SymbolId)> {
    match *r.findings().extra(f) {
        [Arg::File(file), Arg::U32(line), Arg::U32(end), Arg::Symbol(sym)] => Some((file, line, end, sym)),
        _ => None,
    }
}

/// Sorted report positions of PM501 findings whose mirror (the same pair seen from the other
/// copy) comes earlier in the report.
fn duplicate_mirrors(r: &Report<'_>, fs: &[&Finding]) -> Vec<u32> {
    // (file, line, other file, other line, position)
    let mut pairs: Vec<(u32, u32, u32, u32, u32)> = Vec::new();
    for (pos, f) in fs.iter().enumerate() {
        if f.rule == DUPLICATE_CODE
            && let Some((file, line, _, _)) = other_copy(r, f)
        {
            pairs.push((f.loc.file.0, f.loc.line, file.0, line, pos as u32));
        }
    }
    pairs.sort_unstable();
    let mut skip = Vec::new();
    for &(a, al, b, bl, pos) in &pairs {
        let i = pairs.partition_point(|p| (p.0, p.1, p.2, p.3) < (b, bl, a, al));
        if let Some(m) = pairs.get(i)
            && (m.0, m.1, m.2, m.3) == (b, bl, a, al)
            && m.4 < pos
        {
            skip.push(pos);
        }
    }
    skip.sort_unstable();
    skip
}
