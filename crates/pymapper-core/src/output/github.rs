//! GitHub Actions workflow commands: one `::warning file=...,line=...::message` annotation per
//! finding, then the summary footer as plain log lines.
//!
//! Severity maps to the command: error -> `error`, warning -> `warning`, info -> `notice`.
//! Escaping follows the runner: `%`, CR and LF in the message; additionally `:` and `,` in
//! property values.

use std::fmt::Write as _;

use super::buf::Out;
use super::{Options, Report, Summary, concise};
use crate::rules::{FileId, Severity};

pub(super) fn write(r: &Report<'_>, opts: &Options, summary: &Summary, out: &mut Out<'_>) {
    let mut msg = String::new();
    for f in r.iter() {
        let def = f.rule.def();
        out.str(match def.severity {
            Severity::Error => "::error ",
            Severity::Warning => "::warning ",
            Severity::Info => "::notice ",
        });
        if f.loc.file != FileId::NONE {
            out.str("file=");
            escape(r.path(f), true, out);
            if f.loc.line > 0 {
                out.str(",line=");
                out.u32(f.loc.line);
                if f.loc.col > 0 {
                    out.str(",col=");
                    out.u32(f.loc.col);
                }
            }
            out.byte(b',');
        }
        let _ = write!(out, "title={} ", f.rule);
        escape(def.name.as_bytes(), true, out);
        out.str("::");
        r.message(f, &mut msg);
        if f.confidence < 100 {
            let _ = write!(msg, " (confidence {})", f.confidence);
        }
        escape(msg.as_bytes(), false, out);
        out.byte(b'\n');
    }
    if opts.footer {
        concise::footer(r, opts, summary, out);
    }
}

/// Escapes workflow-command data (`property`: also `:` and `,`).
fn escape(s: &[u8], property: bool, out: &mut Out<'_>) {
    let buf = out.buf();
    for &b in s {
        match b {
            b'%' => buf.extend_from_slice(b"%25"),
            b'\r' => buf.extend_from_slice(b"%0D"),
            b'\n' => buf.extend_from_slice(b"%0A"),
            b':' if property => buf.extend_from_slice(b"%3A"),
            b',' if property => buf.extend_from_slice(b"%2C"),
            _ => buf.push(b),
        }
    }
    out.maybe_flush();
}
