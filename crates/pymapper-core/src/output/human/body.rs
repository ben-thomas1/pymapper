//! Line bodies of the human format, per rule family: short symbols, no backticks, metrics from
//! their structured arguments.

use std::io::Write as _;

use super::super::buf::push_u32;
use super::super::lossy_tail;
use super::super::style;
use super::render::{Writer, has_symbol, pad};
use super::{Item, other_copy};
use crate::rules::codes::{FORBIDDEN_IMPORT, INDEPENDENCE_VIOLATION, LAYER_VIOLATION};
use crate::rules::{Arg, ArgKind, ArgResolver, Finding, SymbolId, reason_names};

/// Kind column width (`function`).
const KIND_WIDTH: usize = 8;

impl Writer<'_, '_> {
    /// The message without backticks and without the module prefix of the finding's own
    /// symbol (the file is the heading).
    pub(super) fn plain(&mut self, f: &Finding) {
        self.r.message(f, &mut self.msg);
        self.msg.retain(|c| c != '`');
        self.r.symbol_id(f, &mut self.tmp);
        if let Some(colon) = self.tmp.iter().position(|&b| b == b':')
            && let Ok(prefix) = std::str::from_utf8(&self.tmp[..=colon])
        {
            // In place: `replace_range` shifts the tail, no new string.
            while let Some(i) = self.msg.find(prefix) {
                self.msg.replace_range(i..i + prefix.len(), "");
            }
        }
        self.line.extend_from_slice(self.msg.as_bytes());
    }

    /// Contracts without the importer (it is the file): `imports forbidden repo.order_repo
    /// (contract: models are independent of repo)`. Cycles as their message.
    pub(super) fn architecture(&mut self, f: &Finding) {
        let (before, after) = match f.rule {
            LAYER_VIOLATION => ("imports higher layer ", ""),
            FORBIDDEN_IMPORT => ("imports forbidden ", ""),
            INDEPENDENCE_VIOLATION => ("imports ", ", which must stay independent"),
            _ => return self.plain(f),
        };
        let args = self.r.findings().args(f);
        let [_, target, Arg::Contract(contract)] = *args else { return self.plain(f) };
        let res: &dyn ArgResolver = self.r.ctx();
        self.msg.clear();
        self.msg.push_str(before);
        // Writing into a String cannot fail.
        let _ = match target {
            Arg::Symbol(s) => res.write_symbol(s, &mut self.msg),
            Arg::Name(n) => res.write_name(n, &mut self.msg),
            _ => std::fmt::Write::write_char(&mut self.msg, '?'),
        };
        self.msg.push_str(after);
        self.msg.push_str(" (contract: ");
        let _ = res.write_contract(contract, &mut self.msg);
        self.msg.push(')');
        self.line.extend_from_slice(self.msg.as_bytes());
    }

    /// `symbol  McCabe 14>10 · cognitive 32>15 · ...` for a merged metric item.
    pub(super) fn metrics(&mut self, it: &Item, sym_col: usize) {
        let f = self.first(it);
        self.symbol_column(f, sym_col);
        for k in 0..it.len {
            if k > 0 {
                self.line.extend_from_slice(" · ".as_bytes());
            }
            let g = self.fs[(it.pos + k) as usize];
            self.metric(g);
        }
    }

    /// The short symbol, padded to `width` (0 = not padded), then two spaces.
    fn symbol_column(&mut self, f: &Finding, width: usize) {
        self.r.short_symbol(f, &mut self.tmp);
        self.line.extend_from_slice(&self.tmp);
        if has_symbol(f) {
            let w = style::width(&self.tmp);
            pad(&mut self.line, width.saturating_sub(w) + 2);
        }
    }

    /// One metric: `label value>threshold` (`<` for the maintainability index).
    fn metric(&mut self, f: &Finding) {
        let args = self.r.findings().args(f);
        // (label, index of the value argument (the threshold follows), comparison)
        let pairs: &[(&str, usize, u8)] = match f.rule.code() {
            101 => &[("McCabe", 1, b'>')],
            102 => &[("cognitive", 1, b'>')],
            103 => &[("nesting", 1, b'>')],
            104 | 107 => &[("lines", 1, b'>')],
            105 => &[("params", 1, b'>')],
            106 => &[("maintainability", 1, b'<')],
            201 => &[("fan-out", 1, b'>')],
            202 => &[("distance", 1, b'>')],
            203 => &[("definitions", 1, b'>'), ("coupling", 3, b'>')],
            204 => &[("WMC", 1, b'>'), ("class uses", 3, b'>')],
            _ => return self.plain(f),
        };
        for (k, &(label, at, op)) in pairs.iter().enumerate() {
            if k > 0 {
                self.line.extend_from_slice(" · ".as_bytes());
            }
            self.line.extend_from_slice(label.as_bytes());
            self.line.push(b' ');
            self.number(args.get(at));
            self.line.push(op);
            self.number(args.get(at + 1));
        }
        if f.rule.code() == 202 {
            self.line.extend_from_slice(b" (abstractness ");
            self.number(args.get(3));
            self.line.extend_from_slice(b", instability ");
            self.number(args.get(4));
            self.line.push(b')');
        }
    }

    /// A numeric argument as the messages print it (reals with one decimal).
    fn number(&mut self, a: Option<&Arg>) {
        match a {
            Some(&Arg::U32(v)) => push_u32(&mut self.line, v),
            // Writing into a Vec cannot fail.
            Some(&Arg::F32(v)) => {
                let _ = write!(self.line, "{v:.1}");
            }
            _ => self.line.push(b'?'),
        }
    }

    /// Dead code: `kind  symbol  verdict` (category layout: columns padded; file layout:
    /// `kind symbol  verdict`).
    pub(super) fn dead(&mut self, f: &Finding, sym_col: usize, file_layout: bool) {
        let kind = match f.rule.code() {
            401 => "function",
            402 => "class",
            403 => "method",
            404 => "module",
            _ => return self.plain(f),
        };
        self.line.extend_from_slice(kind.as_bytes());
        if file_layout {
            self.line.push(b' ');
            self.symbol_column(f, 0);
        } else {
            pad(&mut self.line, KIND_WIDTH.saturating_sub(kind.len()) + 2);
            self.symbol_column(f, sym_col);
        }
        // The verdict is the rule's phrase argument without its leading "is ".
        let args = self.r.findings().args(f);
        let phrase = match (args.get(1), f.rule.def().args.get(1)) {
            (Some(&Arg::Phrase(p)), Some(ArgKind::Phrase(words))) => words.get(p as usize).copied(),
            _ => None,
        };
        let verdict = phrase.unwrap_or("?");
        let verdict = verdict.strip_prefix("is ").unwrap_or(verdict);
        for &b in verdict.as_bytes() {
            if b != b'`' {
                self.line.push(b);
            }
        }
    }

    /// File layout duplicate: `receipt_lines  ↔  services/invoice.py:1  invoice_lines  (12 lines)`.
    pub(super) fn pair_body(&mut self, f: &Finding) {
        if other_copy(self.r, f).is_none() {
            return self.plain(f);
        }
        if has_symbol(f) {
            self.symbol_column(f, 0);
        }
        self.pair_rest(f);
    }

    /// Category layout duplicate (no file heading, so both locations):
    /// `  services/pricing.py:39  receipt_lines  ↔  services/invoice.py:1  invoice_lines  (12 lines)`.
    pub(super) fn pair_line(&mut self, it: &Item) {
        let f = self.first(it);
        self.line.clear();
        self.line.extend_from_slice(b"  ");
        self.style.paint(&mut self.line, style::BOLD, self.r.path(f));
        self.line_number(f.loc.line);
        self.line.extend_from_slice(b"  ");
        if other_copy(self.r, f).is_none() {
            self.plain(f);
        } else {
            if has_symbol(f) {
                self.symbol_column(f, 0);
            }
            self.pair_rest(f);
        }
        self.suffix(f);
    }

    /// `↔  other/path.py:1  other_symbol  (12 lines)`.
    fn pair_rest(&mut self, f: &Finding) {
        let Some((file, line, _, sym)) = other_copy(self.r, f) else { return };
        self.line.extend_from_slice("↔  ".as_bytes());
        self.tmp.clear();
        self.r.ctx().write_path(file, &mut self.tmp);
        lossy_tail(&mut self.tmp, 0);
        self.style.paint(&mut self.line, style::BOLD, &self.tmp);
        self.line_number(line);
        if sym != SymbolId::NONE {
            self.line.extend_from_slice(b"  ");
            self.tmp.clear();
            self.r.ctx().write_symbol_id(sym, &mut self.tmp);
            lossy_tail(&mut self.tmp, 0);
            let start = self.tmp.iter().position(|&b| b == b':').map_or(0, |c| c + 1);
            self.line.extend_from_slice(&self.tmp[start..]);
        }
        if let Some(&Arg::U32(n)) = self.r.findings().args(f).first() {
            self.line.extend_from_slice(b"  (");
            push_u32(&mut self.line, n);
            self.line.extend_from_slice(if n == 1 { b" line)" } else { b" lines)" });
        }
    }

    /// `:line` (dim), nothing for 0.
    fn line_number(&mut self, line: u32) {
        if line == 0 {
            return;
        }
        self.style.open(&mut self.line, style::DIM);
        self.line.push(b':');
        push_u32(&mut self.line, line);
        self.style.close(&mut self.line);
    }

    /// ` (confidence 40: unknown-decorator)` (dim) below 100.
    pub(super) fn suffix(&mut self, f: &Finding) {
        if f.confidence >= 100 {
            return;
        }
        self.line.push(b' ');
        self.style.open(&mut self.line, style::DIM);
        self.line.extend_from_slice(b"(confidence ");
        push_u32(&mut self.line, u32::from(f.confidence));
        for (i, name) in reason_names(f).enumerate() {
            self.line.extend_from_slice(if i == 0 { b": " } else { b", " });
            self.line.extend_from_slice(name.as_bytes());
        }
        self.line.push(b')');
        self.style.close(&mut self.line);
    }
}
