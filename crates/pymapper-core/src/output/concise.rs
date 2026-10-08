//! `path:line:col: CODE message`, one finding per line, then the summary footer.

use std::fmt::Write as _;

use super::buf::Out;
use super::{Options, Report, Summary};
use crate::rules::{FileId, Finding};

pub(super) fn write(r: &Report<'_>, opts: &Options, summary: &Summary, out: &mut Out<'_>) {
    let mut msg = String::with_capacity(super::SCRATCH);
    for f in r.iter() {
        location(r, f, out);
        let _ = write!(out, "{} ", f.rule);
        r.message(f, &mut msg);
        // Messages are single-line by construction; keep the format line-oriented regardless.
        for b in msg.bytes() {
            out.byte(if b == b'\n' || b == b'\r' { b' ' } else { b });
        }
        if f.confidence < 100 {
            out.str(" (confidence ");
            out.u32(u32::from(f.confidence));
            out.byte(b')');
        }
        out.byte(b'\n');
    }
    if opts.footer {
        footer(r, opts, summary, out);
    }
}

/// `path:line:col: ` with missing parts left out (`path: ` without a line, nothing without a
/// file).
pub(super) fn location(r: &Report<'_>, f: &Finding, out: &mut Out<'_>) {
    if f.loc.file == FileId::NONE {
        return;
    }
    out.bytes(r.path(f));
    if f.loc.line > 0 {
        out.byte(b':');
        out.u32(f.loc.line);
        if f.loc.col > 0 {
            out.byte(b':');
            out.u32(f.loc.col);
        }
    }
    out.str(": ");
}

fn count(out: &mut Out<'_>, n: u64, one: &str, many: &str) {
    out.u32(n.min(u64::from(u32::MAX)) as u32);
    out.byte(b' ');
    out.str(if n == 1 { one } else { many });
}

/// The summary footer, e.g.
///
/// ```text
/// Found 3 findings in 2 files (1 below min-confidence 80; 120 files checked).
/// Not shown: 2 suppressed, 1 baselined.
/// ```
pub(super) fn footer(r: &Report<'_>, opts: &Options, summary: &Summary, out: &mut Out<'_>) {
    let n = r.len() as u64;
    let below = n - r.gated_count(opts.min_confidence) as u64;
    if n == 0 {
        out.str("All checks passed!");
    } else {
        out.str("Found ");
        count(out, n, "finding", "findings");
        out.str(" in ");
        count(out, r.file_count() as u64, "file", "files");
    }
    let mut paren = false;
    let mut open = |out: &mut Out<'_>| {
        out.str(if paren { "; " } else { " (" });
        paren = true;
    };
    if below > 0 {
        open(out);
        out.u32(below as u32);
        out.str(" below min-confidence ");
        out.u32(u32::from(opts.min_confidence));
    }
    if summary.files_checked > 0 {
        open(out);
        count(out, u64::from(summary.files_checked), "file", "files");
        out.str(" checked");
    }
    if paren {
        out.byte(b')');
    }
    out.str(if n == 0 { "\n" } else { ".\n" });

    // Below-gate findings the filter dropped (those it kept are in the report).
    let hidden_below = summary.filter.below_confidence.saturating_sub(below.min(u64::from(u32::MAX)) as u32);
    let hidden = [
        (summary.filter.suppressed, "suppressed"),
        (r.baselined(), "baselined"),
        (summary.filter.file_ignored, "ignored by per-file-ignores"),
        (hidden_below, "below min-confidence"),
    ];
    let mut first = true;
    for (k, what) in hidden {
        if k == 0 {
            continue;
        }
        out.str(if first { "Not shown: " } else { ", " });
        first = false;
        out.u32(k);
        out.byte(b' ');
        out.str(what);
    }
    if !first {
        out.str(".\n");
    }
}
