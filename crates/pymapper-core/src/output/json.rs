//! JSON output and string escaping.
//!
//! # Schema (version 1)
//!
//! One document; one finding object per line so the output diffs and streams well:
//!
//! ```text
//! {"version":1,"tool":{"name":"pymapper","version":"0.3.0"},"min_confidence":80,"findings":[
//!   {"code":"PM101","name":"complex-function","severity":"warning","confidence":100,
//!    "path":"pkg/mod.py","line":3,"col":5,"symbol":"pkg.mod:f","message":"..."},
//!   ...
//! ],"summary":{"findings":1,"gated":1,"files":1,"files_checked":10,"suppressed":0,
//!   "baselined":0,"file_ignored":0,"below_confidence_hidden":0}}
//! ```
//!
//! (`findings` entries are written on one line each; wrapped here for width.)
//!
//! - `path` is relative to the project root (`null` for project-level findings); `line` and
//!   `col` are 1-based (`null` when unknown; `col` counts bytes); `symbol` is the stable
//!   qualified id (`null` when the finding is not about a symbol).
//! - `severity` is `info`, `warning` or `error`; `confidence` is 0-100.
//! - `gated` counts findings with `confidence >= min_confidence` (these make the exit status 1).
//! - Strings are UTF-8; invalid bytes in paths or names are replaced with U+FFFD.
//!
//! New fields may be added without a version bump; removals and meaning changes bump
//! [`JSON_VERSION`](super::JSON_VERSION).

use super::buf::{Out, push_u32};
use super::{JSON_VERSION, Options, Report, Summary};
use crate::rules::{FileId, SymbolId};

/// Appends `s` as a JSON string literal (with quotes). Invalid UTF-8 becomes `�`.
pub fn push_str(out: &mut Vec<u8>, s: &[u8]) {
    out.push(b'"');
    match std::str::from_utf8(s) {
        Ok(_) => push_escaped(out, s),
        Err(_) => {
            for chunk in s.utf8_chunks() {
                push_escaped(out, chunk.valid().as_bytes());
                if !chunk.invalid().is_empty() {
                    out.extend_from_slice(b"\\ufffd");
                }
            }
        }
    }
    out.push(b'"');
}

/// Bytes that need escaping in a JSON string: controls, `"`, `\` (and DEL, for readability).
static ESCAPE: [bool; 256] = {
    let mut t = [false; 256];
    let mut b = 0;
    while b < 0x20 {
        t[b] = true;
        b += 1;
    }
    t[b'"' as usize] = true;
    t[b'\\' as usize] = true;
    t[0x7f] = true;
    t
};

/// Escapes valid UTF-8 (no quotes): runs of plain bytes are copied at once.
fn push_escaped(out: &mut Vec<u8>, s: &[u8]) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut run = 0;
    for (i, &b) in s.iter().enumerate() {
        if !ESCAPE[b as usize] {
            continue;
        }
        let esc: &[u8] = match b {
            b'"' => b"\\\"",
            b'\\' => b"\\\\",
            b'\n' => b"\\n",
            b'\r' => b"\\r",
            b'\t' => b"\\t",
            _ => b"",
        };
        out.extend_from_slice(&s[run..i]);
        run = i + 1;
        if esc.is_empty() {
            out.extend_from_slice(&[b'\\', b'u', b'0', b'0', HEX[(b >> 4) as usize], HEX[(b & 0xf) as usize]]);
        } else {
            out.extend_from_slice(esc);
        }
    }
    out.extend_from_slice(&s[run..]);
}

/// `"key":` with a leading comma unless `first`.
pub(super) fn key(out: &mut Vec<u8>, k: &str, first: bool) {
    if !first {
        out.push(b',');
    }
    out.push(b'"');
    out.extend_from_slice(k.as_bytes());
    out.extend_from_slice(b"\":");
}

/// `"key":value` for a number, with a leading comma.
pub(super) fn key_u32(out: &mut Vec<u8>, k: &str, v: u32) {
    key(out, k, false);
    push_u32(out, v);
}

/// A number, or `null` for 0.
fn u32_or_null(out: &mut Vec<u8>, v: u32) {
    if v == 0 {
        out.extend_from_slice(b"null");
    } else {
        push_u32(out, v);
    }
}

pub(super) fn write_report(r: &Report<'_>, opts: &Options, summary: &Summary, out: &mut Out<'_>) {
    let mut msg = String::with_capacity(super::SCRATCH);
    let mut sym = Vec::with_capacity(super::SCRATCH);
    {
        let b = out.buf();
        b.extend_from_slice(b"{\"version\":");
        push_u32(b, JSON_VERSION);
        b.extend_from_slice(b",\"tool\":{\"name\":\"pymapper\",\"version\":");
        push_str(b, env!("CARGO_PKG_VERSION").as_bytes());
        b.push(b'}');
        key_u32(b, "min_confidence", u32::from(opts.min_confidence));
        b.extend_from_slice(b",\"findings\":[");
    }
    for (i, f) in r.iter().enumerate() {
        r.message(f, &mut msg);
        r.symbol_id(f, &mut sym);
        let def = f.rule.def();
        let b = out.buf();
        b.extend_from_slice(if i == 0 { b"\n  {\"code\":\"PM" } else { b",\n  {\"code\":\"PM" });
        let code = f.rule.code();
        b.extend_from_slice(&[b'0' + (code / 100 % 10) as u8, b'0' + (code / 10 % 10) as u8, b'0' + (code % 10) as u8]);
        b.push(b'"');
        key(b, "name", false);
        push_str(b, def.name.as_bytes());
        key(b, "severity", false);
        push_str(b, def.severity.as_str().as_bytes());
        key_u32(b, "confidence", u32::from(f.confidence));
        key(b, "path", false);
        if f.loc.file == FileId::NONE {
            b.extend_from_slice(b"null");
        } else {
            push_str(b, r.path(f));
        }
        key(b, "line", false);
        u32_or_null(b, f.loc.line);
        key(b, "col", false);
        u32_or_null(b, f.loc.col);
        key(b, "symbol", false);
        if f.symbol == SymbolId::NONE {
            b.extend_from_slice(b"null");
        } else {
            push_str(b, &sym);
        }
        key(b, "message", false);
        push_str(b, msg.as_bytes());
        b.push(b'}');
        out.maybe_flush();
    }
    let gated = r.gated_count(opts.min_confidence);
    let below_shown = (r.len() - gated) as u32;
    let b = out.buf();
    b.extend_from_slice(if r.is_empty() { b"],\"summary\":{" } else { b"\n],\"summary\":{" });
    key(b, "findings", true);
    push_u32(b, r.len() as u32);
    key_u32(b, "gated", gated as u32);
    key_u32(b, "files", r.file_count() as u32);
    key_u32(b, "files_checked", summary.files_checked);
    key_u32(b, "suppressed", summary.filter.suppressed);
    key_u32(b, "baselined", r.baselined());
    key_u32(b, "file_ignored", summary.filter.file_ignored);
    key_u32(b, "below_confidence_hidden", summary.filter.below_confidence.saturating_sub(below_shown));
    b.extend_from_slice(b"}}\n");
    out.maybe_flush();
}
