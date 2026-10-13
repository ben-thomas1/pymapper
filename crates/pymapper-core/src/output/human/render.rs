//! Layouts of the human format: sections or files, headings, the line:col column, the id
//! column and the footer. Line bodies are in `body.rs`.

use super::super::buf::{Out, push_code, push_u32};
use super::super::style::{self, Style};
use super::super::{FindingIds, Options, Report, Summary};
use super::{Item, LABEL_WIDTH, Section};
use crate::rules::codes::LONG_MODULE;
use crate::rules::{FileId, Finding, SymbolId};

/// Short symbols longer than this do not widen the symbol column (they get two spaces).
const SYMBOL_COLUMN_MAX: usize = 40;
/// Heading of findings without a file (project-level).
const NO_FILE: &[u8] = b"(project)";

/// Column widths of a group of lines (a section, or a file in the file layout).
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct Cols {
    line: usize,
    col: usize,
    /// Symbol column (0 = not padded).
    pub(super) sym: usize,
}

/// Renders lines into one reused buffer, then hands each to the output.
pub(in crate::output) struct Writer<'r, 'a> {
    pub(super) r: &'r Report<'a>,
    pub(super) fs: &'r [&'a Finding],
    ids: FindingIds,
    pub(super) style: Style,
    width: usize,
    /// The line being built (with escapes when styled).
    pub(super) line: Vec<u8>,
    /// Scratch: symbols, paths, ids.
    pub(super) tmp: Vec<u8>,
    /// Scratch: messages.
    pub(super) msg: String,
}

impl<'r, 'a> Writer<'r, 'a> {
    pub(super) fn new(r: &'r Report<'a>, fs: &'r [&'a Finding], color: bool, width: usize) -> Self {
        Writer {
            r,
            fs,
            ids: r.ids(),
            style: Style { on: color },
            width,
            line: Vec::with_capacity(super::super::SCRATCH),
            tmp: Vec::with_capacity(super::super::SCRATCH),
            msg: String::with_capacity(super::super::SCRATCH),
        }
    }

    #[inline]
    pub(super) fn first(&self, it: &Item) -> &'a Finding {
        self.fs[it.pos as usize]
    }

    /// Category layout: `items` sorted by section.
    pub(super) fn by_category(&mut self, items: &[Item], out: &mut Out<'_>) {
        let counts = tally(items, self.fs);
        let mut i = 0;
        while i < items.len() {
            let section = items[i].section;
            let mut j = i + 1;
            while j < items.len() && items[j].section == section {
                j += 1;
            }
            let group = &items[i..j];
            if i > 0 {
                out.byte(b'\n');
            }
            self.line.clear();
            self.style.paint(&mut self.line, section.color(), section.title().as_bytes());
            self.line.extend_from_slice(b"  ");
            count_text(section, counts[section as usize], &mut self.line);
            self.flush_line(out);
            if section == Section::Duplication {
                // Pairs name both files; no file headings.
                for it in group {
                    self.pair_line(it);
                    self.end(it, out);
                }
            } else {
                let cols = self.cols(group, matches!(section, Section::Complexity | Section::Coupling | Section::DeadCode));
                let mut file = None;
                for it in group {
                    let f = self.first(it);
                    if file != Some(f.loc.file) {
                        file = Some(f.loc.file);
                        self.heading(f, b"  ", out);
                    }
                    self.item_line(it, cols, false);
                    self.end(it, out);
                }
            }
            i = j;
        }
    }

    /// File layout: `items` in report order (so grouped by file).
    pub(super) fn by_file(&mut self, items: &[Item], out: &mut Out<'_>) {
        let mut i = 0;
        while i < items.len() {
            let file = self.first(&items[i]).loc.file;
            let mut j = i + 1;
            while j < items.len() && self.first(&items[j]).loc.file == file {
                j += 1;
            }
            if i > 0 {
                out.byte(b'\n');
            }
            let f = self.first(&items[i]);
            self.heading(f, b"", out);
            let cols = self.cols(&items[i..j], false);
            for it in &items[i..j] {
                self.item_line(it, cols, true);
                self.end(it, out);
            }
            i = j;
        }
    }

    /// A file heading: the path in bold.
    fn heading(&mut self, f: &Finding, indent: &[u8], out: &mut Out<'_>) {
        self.line.clear();
        self.line.extend_from_slice(indent);
        let path = if f.loc.file == FileId::NONE { NO_FILE } else { self.r.path(f) };
        self.style.paint(&mut self.line, style::BOLD, path);
        self.flush_line(out);
    }

    /// Column widths over `group` (line and column at least 2 digits, as in the mockup).
    fn cols(&mut self, group: &[Item], sym: bool) -> Cols {
        let mut c = Cols { line: 2, col: 2, sym: 0 };
        for it in group {
            let f = self.first(it);
            c.line = c.line.max(digits(f.loc.line));
            c.col = c.col.max(digits(f.loc.col));
            if sym {
                self.r.short_symbol(f, &mut self.tmp);
                let w = style::width(&self.tmp);
                if w <= SYMBOL_COLUMN_MAX {
                    c.sym = c.sym.max(w);
                }
            }
        }
        c
    }

    /// Starts a finding line: indent, `line:col` (dim), the category column in the file
    /// layout, then the body and the low-confidence suffix.
    fn item_line(&mut self, it: &Item, cols: Cols, file_layout: bool) {
        let f = self.first(it);
        self.line.clear();
        self.line.extend_from_slice(b"    ");
        if f.loc.line == 0 {
            pad(&mut self.line, cols.line + 1 + cols.col);
        } else {
            pad(&mut self.line, cols.line - digits(f.loc.line));
            self.style.open(&mut self.line, style::DIM);
            push_u32(&mut self.line, f.loc.line);
            self.line.push(b':');
            let mut used = 0;
            if f.loc.col > 0 {
                push_u32(&mut self.line, f.loc.col);
                used = digits(f.loc.col);
            }
            self.style.close(&mut self.line);
            pad(&mut self.line, cols.col - used);
        }
        self.line.extend_from_slice(b"  ");
        if file_layout {
            let label = it.section.label();
            self.style.paint(&mut self.line, it.section.color(), label.as_bytes());
            pad(&mut self.line, LABEL_WIDTH.saturating_sub(label.len()) + 2);
        }
        match it.section {
            Section::Architecture => self.architecture(f),
            Section::Complexity | Section::Coupling => self.metrics(it, cols.sym),
            Section::Duplication => self.pair_body(f),
            Section::DeadCode => self.dead(f, cols.sym, file_layout),
            Section::UnusedImports => {
                self.r.short_symbol(f, &mut self.tmp);
                self.line.extend_from_slice(&self.tmp);
            }
            Section::Dependencies | Section::Other => self.plain(f),
        }
        self.suffix(f);
    }

    /// Ends the line with the item's id (right-aligned when it fits) and writes it.
    pub(super) fn end(&mut self, it: &Item, out: &mut Out<'_>) {
        self.tmp.clear();
        self.ids.get(it.pos as usize).push(&mut self.tmp);
        for k in 1..it.len {
            self.tmp.extend_from_slice(if k == 1 { b" +" } else { b"," });
            push_code(&mut self.tmp, self.fs[(it.pos + k) as usize].rule.code());
        }
        let used = style::width(&self.line);
        let id = self.tmp.len();
        let gap = if used + 2 + id <= self.width { self.width - used - id } else { 2 };
        pad(&mut self.line, gap);
        self.style.paint(&mut self.line, style::DIM, &self.tmp);
        self.flush_line(out);
    }

    /// Writes the line (newlines in names become spaces) and a newline.
    fn flush_line(&mut self, out: &mut Out<'_>) {
        for b in &mut self.line {
            if *b == b'\n' || *b == b'\r' {
                *b = b' ';
            }
        }
        out.bytes(&self.line);
        out.byte(b'\n');
    }
}

/// Appends `n` spaces.
#[inline]
pub(super) fn pad(buf: &mut Vec<u8>, n: usize) {
    buf.resize(buf.len() + n, b' ');
}

/// Decimal digits of `v` (0 for 0: nothing is shown).
fn digits(v: u32) -> usize {
    if v == 0 { 0 } else { v.ilog10() as usize + 1 }
}

/// Items and module items (PM107) per section, indexed by `Section as usize`.
type Counts = [(u32, u32); 8];

fn tally(items: &[Item], fs: &[&Finding]) -> Counts {
    let mut c: Counts = [(0, 0); 8];
    for it in items {
        let e = &mut c[it.section as usize];
        e.0 += 1;
        if fs[it.pos as usize].rule == LONG_MODULE {
            e.1 += 1;
        }
    }
    c
}

fn plural(buf: &mut Vec<u8>, n: u32, one: &str, many: &str) {
    push_u32(buf, n);
    buf.push(b' ');
    buf.extend_from_slice(if n == 1 { one } else { many }.as_bytes());
}

/// A section's count: functions (and modules) for complexity, pairs for duplication, items
/// otherwise.
fn count_text(section: Section, (n, modules): (u32, u32), buf: &mut Vec<u8>) {
    match section {
        Section::Complexity => {
            let functions = n - modules;
            if functions > 0 || modules == 0 {
                plural(buf, functions, "function", "functions");
            }
            if modules > 0 {
                if functions > 0 {
                    buf.extend_from_slice(b", ");
                }
                plural(buf, modules, "module", "modules");
            }
        }
        Section::Duplication => plural(buf, n, "pair", "pairs"),
        _ => push_u32(buf, n),
    }
}

/// The footer, e.g.
///
/// ```text
/// 26 findings in 15 files (23 files checked) · architecture 2 · complexity 1 function · dead code 16
/// 10 more below confidence 80 (use --min-confidence 0 to show them)
/// Not shown: 1 suppressed, 2 baselined.
/// ```
pub(super) fn footer(r: &Report<'_>, opts: &Options, summary: &Summary, items: &[Item], fs: &[&Finding], out: &mut Out<'_>) {
    let mut b: Vec<u8> = Vec::with_capacity(256);
    let n = r.len() as u32;
    let below = n - r.gated_count(opts.min_confidence) as u32;
    if n == 0 {
        b.extend_from_slice(b"All checks passed!");
    } else {
        plural(&mut b, n, "finding", "findings");
        b.extend_from_slice(b" in ");
        plural(&mut b, r.file_count() as u32, "file", "files");
    }
    let mut notes = Vec::with_capacity(2);
    if below > 0 {
        notes.push((below, " below confidence "));
    }
    if summary.files_checked > 0 {
        notes.push((summary.files_checked, ""));
    }
    for (k, &(v, what)) in notes.iter().enumerate() {
        b.extend_from_slice(if k == 0 { b" (" } else { b"; " });
        if what.is_empty() {
            plural(&mut b, v, "file checked", "files checked");
        } else {
            push_u32(&mut b, v);
            b.extend_from_slice(what.as_bytes());
            push_u32(&mut b, u32::from(opts.min_confidence));
        }
    }
    if !notes.is_empty() {
        b.push(b')');
    }
    let counts = tally(items, fs);
    for s in SECTIONS {
        let c = counts[s as usize];
        if c.0 > 0 {
            b.extend_from_slice(" · ".as_bytes());
            b.extend_from_slice(s.tally().as_bytes());
            b.push(b' ');
            count_text(s, c, &mut b);
        }
    }
    b.push(b'\n');
    // Below-gate findings the filter dropped (those it kept are in the report).
    let hidden_below = summary.filter.below_confidence.saturating_sub(below);
    if hidden_below > 0 {
        push_u32(&mut b, hidden_below);
        b.extend_from_slice(b" more below confidence ");
        push_u32(&mut b, u32::from(opts.min_confidence));
        b.extend_from_slice(b" (use --min-confidence 0 to show them)\n");
    }
    let hidden = [
        (summary.filter.suppressed, "suppressed"),
        (r.baselined(), "baselined"),
        (summary.filter.file_ignored, "ignored by per-file-ignores"),
    ];
    let mut first = true;
    for (k, what) in hidden {
        if k == 0 {
            continue;
        }
        b.extend_from_slice(if first { b"Not shown: " } else { b", " });
        first = false;
        push_u32(&mut b, k);
        b.push(b' ');
        b.extend_from_slice(what.as_bytes());
    }
    if !first {
        b.extend_from_slice(b".\n");
    }
    out.bytes(&b);
}

/// Every section, in output order.
const SECTIONS: [Section; 8] = [
    Section::Architecture,
    Section::Dependencies,
    Section::Complexity,
    Section::Coupling,
    Section::Duplication,
    Section::DeadCode,
    Section::UnusedImports,
    Section::Other,
];

/// Whether a finding has a symbol.
#[inline]
pub(super) fn has_symbol(f: &Finding) -> bool {
    f.symbol != SymbolId::NONE
}
