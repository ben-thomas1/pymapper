//! Rendering of one llm item: an evidence line (id, location, symbol, verdict or metrics,
//! confidence), optional detail lines, then `fix: ... · verify: ...`.

use std::fmt::Write as _;

use super::group::{COMPLEXITY, COUPLING, DEAD, DUPLICATION, Groups, IMPORTS, Item, Kind};
use crate::output::buf::Out;
use crate::output::{FindingIds, Report, lossy_tail};
use crate::rules::codes::*;
use crate::rules::{Arg, ArgKind, ConfidenceModel, FileId, Finding, SymbolId, reason_names};

/// Most names in one `grep -E` verification command.
const MAX_GREP_NAMES: usize = 10;

/// Renders items; scratch buffers are reused across items (no allocation per finding once
/// they have grown).
pub(super) struct Writer<'r, 'a> {
    r: &'r Report<'a>,
    fs: &'r [&'a Finding],
    ids: &'r FindingIds,
    g: &'r Groups,
    /// The project root as reached from the working directory (`grep` commands).
    root: &'r str,
    msg: String,
    fix: String,
    sym: Vec<u8>,
    scratch: Vec<u8>,
    names: Vec<u8>,
}

impl<'r, 'a> Writer<'r, 'a> {
    pub fn new(r: &'r Report<'a>, fs: &'r [&'a Finding], ids: &'r FindingIds, g: &'r Groups, root: &'r str) -> Self {
        Writer {
            r,
            fs,
            ids,
            g,
            root,
            msg: String::with_capacity(crate::output::SCRATCH),
            fix: String::with_capacity(crate::output::SCRATCH),
            sym: Vec::with_capacity(crate::output::SCRATCH),
            scratch: Vec::with_capacity(crate::output::SCRATCH),
            names: Vec::with_capacity(crate::output::SCRATCH),
        }
    }

    pub fn item(&mut self, item: &Item, out: &mut Out<'_>) {
        let members = self.g.of(item);
        let first = members[0];
        self.id(first, out);
        match item.section {
            COMPLEXITY | COUPLING if self.fs[first as usize].symbol != SymbolId::NONE => self.metrics(members, out),
            DUPLICATION if self.fs[first as usize].rule == DUPLICATE_CODE => self.pair(first, out),
            DEAD => self.dead(item.kind, members, out),
            IMPORTS => self.imports(members, out),
            _ => self.generic(item.kind, members, out),
        }
        out.maybe_flush();
    }

    fn f(&self, pos: u32) -> &'a Finding {
        self.fs[pos as usize]
    }

    /// `PM401-3fa2 `.
    fn id(&self, pos: u32, out: &mut Out<'_>) {
        self.ids.get(pos as usize).push(out.buf());
        out.byte(b' ');
    }

    /// `path:line` (`path` without a line, nothing without a file).
    fn loc(&self, f: &Finding, out: &mut Out<'_>) {
        if f.loc.file == FileId::NONE {
            return;
        }
        out.bytes(self.r.path(f));
        if f.loc.line > 0 {
            out.byte(b':');
            out.u32(f.loc.line);
        }
    }

    /// ` (conf 50: reason, ...)` for scored rules.
    fn conf(&self, f: &Finding, out: &mut Out<'_>) {
        if f.rule.def().confidence == ConfidenceModel::Certain {
            return;
        }
        out.str(" (conf ");
        out.u32(u32::from(f.confidence));
        if f.confidence < 100 {
            for (i, name) in reason_names(f).enumerate() {
                out.str(if i == 0 { ": " } else { ", " });
                out.str(name);
            }
        }
        out.byte(b')');
    }

    /// `  fix: <hint of pos>[ (likewise for the other N)] · verify: ` (the caller writes the
    /// command and the newline).
    fn fix(&mut self, pos: u32, others: usize, out: &mut Out<'_>) {
        let f = self.f(pos);
        self.r.fix_plain(f, &mut self.fix, &mut self.scratch);
        out.str("  fix: ");
        out.str(&self.fix);
        if others > 0 {
            out.str(" (likewise for the other");
            if others > 1 {
                out.byte(b' ');
                out.u32(others as u32);
            }
            out.byte(b')');
        }
        out.str(" · verify: ");
    }

    fn explain(&self, pos: u32, out: &mut Out<'_>) {
        out.str("pymapper explain ");
        self.ids.get(pos as usize).push(out.buf());
        out.byte(b'\n');
    }

    /// The message (plain) after the location; cycles, contracts, dependencies, and metric
    /// findings without a symbol.
    fn generic(&mut self, kind: Kind, members: &[u32], out: &mut Out<'_>) {
        let first = members[0];
        let f = self.f(first);
        self.loc(f, out);
        if f.loc.file != FileId::NONE {
            out.byte(b' ');
        }
        self.r.message_plain(f, &mut self.msg, &mut self.scratch);
        out.str(&self.msg);
        if kind == Kind::Sites {
            out.str("; also imported at ");
            for (i, &pos) in members[1..].iter().enumerate() {
                if i > 0 {
                    out.str(", ");
                }
                self.loc(self.f(pos), out);
            }
        }
        self.conf(f, out);
        out.byte(b'\n');
        self.fix(first, 0, out);
        self.explain(first, out);
    }

    /// Metric findings of one symbol: `path:line-end symbol: McCabe 14>10, cognitive 32>15
    /// (PM101,PM102)`; the fix of the worst overshoot.
    fn metrics(&mut self, members: &[u32], out: &mut Out<'_>) {
        let r = self.r;
        let first = self.f(members[0]);
        self.loc(first, out);
        let end = r.end_line(first);
        if first.loc.line > 0 && end > first.loc.line {
            out.byte(b'-');
            out.u32(end);
        }
        out.byte(b' ');
        r.short_symbol(first, &mut self.sym);
        out.bytes(&self.sym);
        out.str(": ");
        let mut worst = members[0];
        let mut worst_by = -1f32;
        for (i, &pos) in members.iter().enumerate() {
            let f = self.f(pos);
            if i > 0 {
                out.str(", ");
            }
            let args = r.findings().args(f);
            for (k, &(label, at, sep)) in labels(f.rule.code()).iter().enumerate() {
                if k > 0 {
                    out.str(", ");
                }
                out.str(label);
                out.byte(b' ');
                number(args.get(at), out);
                out.str(sep);
                number(args.get(at + 1), out);
            }
            let by = super::group::overshoot(r, f);
            if by > worst_by {
                worst_by = by;
                worst = pos;
            }
        }
        if members.len() > 1 {
            out.str(" (");
            for (i, &pos) in members.iter().enumerate() {
                if i > 0 {
                    out.byte(b',');
                }
                crate::output::buf::push_code(out.buf(), self.f(pos).rule.code());
            }
            out.byte(b')');
        }
        out.byte(b'\n');
        self.fix(worst, 0, out);
        // Module metrics: no callers to count.
        let module = matches!(first.rule, LONG_MODULE | HIGH_FAN_OUT | MAIN_SEQUENCE_DISTANCE | GOD_MODULE);
        if module {
            self.explain(members[0], out);
        } else {
            r.symbol_id(first, &mut self.sym);
            out.str("pymapper callers ");
            out.bytes(&self.sym);
            out.byte(b'\n');
        }
    }

    /// One duplicate pair: `12 duplicated lines: a.py:1-12 f = b.py:39-50 g`.
    fn pair(&mut self, pos: u32, out: &mut Out<'_>) {
        let r = self.r;
        let f = self.f(pos);
        let args = r.findings().args(f);
        if let Some(&Arg::U32(lines)) = args.first() {
            out.u32(lines);
            out.str(" duplicated lines: ");
        }
        self.loc(f, out);
        let end = r.end_line(f);
        if end > f.loc.line {
            out.byte(b'-');
            out.u32(end);
        }
        r.short_symbol(f, &mut self.sym);
        if !self.sym.is_empty() {
            out.byte(b' ');
            out.bytes(&self.sym);
        }
        out.str(" = ");
        if let [Arg::File(file), Arg::U32(line), Arg::U32(end), Arg::Symbol(other)] = *r.findings().extra(f) {
            self.scratch.clear();
            r.ctx().write_path(file, &mut self.scratch);
            lossy_tail(&mut self.scratch, 0);
            out.bytes(&self.scratch);
            out.byte(b':');
            out.u32(line);
            if end > line {
                out.byte(b'-');
                out.u32(end);
            }
            if other != SymbolId::NONE {
                self.sym.clear();
                r.ctx().write_symbol_id(other, &mut self.sym);
                lossy_tail(&mut self.sym, 0);
                let short = self.sym.iter().position(|&b| b == b':').map_or(0, |c| c + 1);
                out.byte(b' ');
                out.bytes(&self.sym[short..]);
            }
        } else {
            self.r.message_plain(f, &mut self.msg, &mut self.scratch);
            out.str(&self.msg);
        }
        self.conf(f, out);
        out.byte(b'\n');
        self.fix(pos, 0, out);
        self.explain(pos, out);
    }
}

impl Writer<'_, '_> {
    /// Dead code: a chain (root, then "also dead if it goes"), a group of one file, or one
    /// symbol; verified with `grep -rnw` on their names.
    fn dead(&mut self, kind: Kind, members: &[u32], out: &mut Out<'_>) {
        let first = members[0];
        match kind {
            Kind::DeadGroup => {
                let f = self.f(first);
                out.bytes(self.r.path(f));
                out.str(": ");
                for (i, &pos) in members.iter().enumerate() {
                    let m = self.f(pos);
                    if i > 0 {
                        out.str(", ");
                    }
                    self.r.short_symbol(m, &mut self.sym);
                    out.bytes(&self.sym);
                    out.byte(b':');
                    out.u32(m.loc.line);
                    out.str(" (");
                    out.str(kind_word(m));
                    out.byte(b')');
                }
                out.str(": ");
                out.str(verdict(self.r, f));
                self.conf(f, out);
                out.byte(b'\n');
                self.fix(first, members.len() - 1, out);
            }
            _ => {
                self.dead_head(first, out);
                if kind == Kind::Chain {
                    out.str("  also dead if it goes: ");
                    let mut file = FileId::NONE;
                    for (i, &pos) in members[1..].iter().enumerate() {
                        let m = self.f(pos);
                        if i == 0 || m.loc.file != file {
                            if i > 0 {
                                out.str("; ");
                            }
                            file = m.loc.file;
                            out.bytes(self.r.path(m));
                            out.str(": ");
                        } else {
                            out.str(", ");
                        }
                        if m.rule == UNUSED_MODULE {
                            out.str("the module");
                        } else {
                            self.r.short_symbol(m, &mut self.sym);
                            out.bytes(&self.sym);
                            out.byte(b':');
                            out.u32(m.loc.line);
                        }
                        if m.confidence < 100 {
                            out.str(" (conf ");
                            out.u32(u32::from(m.confidence));
                            out.byte(b')');
                        }
                    }
                    out.byte(b'\n');
                }
                self.fix(first, 0, out);
            }
        }
        self.grep_names(members, true, out);
    }

    /// `path:line Class.method (method): never used (conf 100)`; a module: `path: module never
    /// imported (conf 100)`.
    fn dead_head(&mut self, pos: u32, out: &mut Out<'_>) {
        let f = self.f(pos);
        if f.rule == UNUSED_MODULE {
            out.bytes(self.r.path(f));
            out.str(": module ");
        } else {
            self.loc(f, out);
            out.byte(b' ');
            self.r.short_symbol(f, &mut self.sym);
            out.bytes(&self.sym);
            out.str(" (");
            out.str(kind_word(f));
            out.str("): ");
        }
        out.str(verdict(self.r, f));
        self.conf(f, out);
        out.byte(b'\n');
    }

    /// Unused imports of one file: `path:1 json` or `path: json:1, os:3`.
    fn imports(&mut self, members: &[u32], out: &mut Out<'_>) {
        let first = members[0];
        let f = self.f(first);
        if members.len() == 1 {
            self.loc(f, out);
            out.byte(b' ');
            self.r.short_symbol(f, &mut self.sym);
            out.bytes(&self.sym);
        } else {
            out.bytes(self.r.path(f));
            out.str(": ");
            for (i, &pos) in members.iter().enumerate() {
                let m = self.f(pos);
                if i > 0 {
                    out.str(", ");
                }
                self.r.short_symbol(m, &mut self.sym);
                out.bytes(&self.sym);
                out.byte(b':');
                out.u32(m.loc.line);
            }
        }
        self.conf(f, out);
        out.byte(b'\n');
        self.fix(first, members.len() - 1, out);
        self.grep_names(members, false, out);
    }

    /// `grep -rnw --exclude-dir=.venv name <root>` (`project`) or `grep -nw name <file>`, with
    /// `-E 'a|b'` for several names (at most [`MAX_GREP_NAMES`]).
    fn grep_names(&mut self, members: &[u32], project: bool, out: &mut Out<'_>) {
        self.names.clear();
        let mut count = 0;
        for &pos in members {
            if count == MAX_GREP_NAMES {
                break;
            }
            let f = self.f(pos);
            self.r.short_symbol(f, &mut self.sym);
            // The last dotted segment, without a `#N` duplicate suffix.
            let name = self.sym.split(|&b| b == b'#').next().unwrap_or(&[]);
            let name = name.rsplit(|&b| b == b'.').next().unwrap_or(name);
            if name.is_empty() || self.names.split(|&b| b == b'|').any(|n| n == name) {
                continue;
            }
            if count > 0 {
                self.names.push(b'|');
            }
            self.names.extend_from_slice(name);
            count += 1;
        }
        out.str(if project { "grep -rnw" } else { "grep -nw" });
        out.str(if count > 1 { "E " } else { " " });
        if project {
            out.str("--exclude-dir=.venv ");
        }
        if count > 1 {
            out.byte(b'\'');
            out.bytes(&self.names);
            out.byte(b'\'');
        } else {
            out.bytes(&self.names);
        }
        out.byte(b' ');
        if project {
            out.str(self.root);
        } else {
            out.bytes(self.r.path(self.f(members[0])));
        }
        out.byte(b'\n');
    }
}

/// `function`, `class`, `method` or `module`.
fn kind_word(f: &Finding) -> &'static str {
    match f.rule {
        UNUSED_CLASS => "class",
        UNUSED_METHOD => "method",
        UNUSED_MODULE => "module",
        _ => "function",
    }
}

/// The verdict of a dead-code finding without its leading `is `: `never used`, `only used by
/// unused code`, `has no resolved callers (...)`.
fn verdict(r: &Report<'_>, f: &Finding) -> &'static str {
    let def = f.rule.def();
    match (r.findings().args(f).get(1), def.args.get(1)) {
        (Some(&Arg::Phrase(i)), Some(ArgKind::Phrase(words))) => {
            let w = words.get(i as usize).copied().unwrap_or("unused");
            w.strip_prefix("is ").unwrap_or(w)
        }
        _ => "unused",
    }
}

/// The metrics a rule's message carries: label, index of the value argument (the limit
/// follows it) and the comparison shown.
fn labels(code: u16) -> &'static [(&'static str, usize, &'static str)] {
    match code {
        101 => &[("McCabe", 1, ">")],
        102 => &[("cognitive", 1, ">")],
        103 => &[("nesting", 1, ">")],
        104 => &[("lines", 1, ">")],
        105 => &[("params", 1, ">")],
        106 => &[("maintainability", 1, "<")],
        107 => &[("module lines", 1, ">")],
        201 => &[("fan-out", 1, ">")],
        202 => &[("distance", 1, ">")],
        203 => &[("symbols", 1, ">"), ("coupling", 3, ">")],
        204 => &[("WMC", 1, ">"), ("class fan-out", 3, ">")],
        _ => &[("value", 1, ">")],
    }
}

/// A metric argument: integers as is, reals with at most two decimals.
fn number(a: Option<&Arg>, out: &mut Out<'_>) {
    match a {
        Some(&Arg::U32(v)) => out.u32(v),
        Some(&Arg::F32(v)) if v.is_finite() => {
            let _ = write!(out, "{}", (f64::from(v) * 100.0).round() / 100.0);
        }
        _ => out.byte(b'?'),
    }
}
