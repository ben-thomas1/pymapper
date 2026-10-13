//! JSON output and string escaping.
//!
//! # Schema (version 2)
//!
//! One document; one finding object per line so the output diffs and streams well:
//!
//! ```text
//! {"version":2,"tool":{"name":"pymapper","version":"0.4.0"},"root":"/abs/project",
//!  "min_confidence":80,"findings":[
//!   {"code":"PM101","id":"PM101-0b2e","name":"complex-function","severity":"warning",
//!    "confidence":100,"path":"pkg/mod.py","line":3,"col":5,"end_line":40,"symbol":"pkg.mod:f",
//!    "message":"...","value":14,"threshold":10,"fix":"extract branches of `pkg.mod:f` ..."},
//!   {"code":"PM301",...,"message":"import cycle (3 modules): a -> b -> a",
//!    "cycle":{"size":3,"members":["a","b","c"],"witness":["a","b"]}},
//!   {"code":"PM501",...,"end_line":21,"symbol":"pkg.mod:g","message":"8 lines duplicated in ...",
//!    "other":{"path":"pkg/other.py","line":40,"end_line":47,"symbol":"pkg.other:h"}},
//!   ...
//! ],"summary":{"findings":1,"gated":1,"files":1,"files_checked":10,"suppressed":0,
//!   "baselined":0,"file_ignored":0,"below_confidence_hidden":0}}
//! ```
//!
//! (`findings` entries are written on one line each; wrapped here for width.)
//!
//! - `root`: the absolute project root, which `path` values are relative to (`null` when
//!   unknown).
//! - `id`: the stable finding id (`PM401-3fa2`, see [`super::ids`]), as shown by the text
//!   formats and accepted by `pymapper explain`.
//! - `fix`: a one-line fix hint, the rule's template ([`crate::rules::RuleDef::fix`]) filled with
//!   the finding's facts (the same text as the `--llm` output, with qualified symbols).
//! - `path` is relative to the project root (`null` for project-level findings); `line` and
//!   `col` are 1-based (`null` when unknown; `col` counts bytes); `symbol` is the stable
//!   qualified id (`null` when the finding is not about a symbol); for PM405 it is
//!   `pkg.mod:<bound name>`.
//! - `end_line`: last line of the reported span (the definition for function / class
//!   findings, the whole file for module metrics, the copy for PM501, else `line`).
//! - `value` / `threshold` (PM1xx, PM2xx): the measured value and its limit (the first pair of
//!   the message for PM203 / PM204, which check two limits); reals have at most two decimals.
//! - `cycle` (PM301): `size` (modules in the strongly connected component), `members` (all of
//!   them) and `witness` (the cycle in the message, without repeating its first module).
//! - `other` (PM501): the other copy of the reported pair.
//! - `severity` is `info`, `warning` or `error`; `confidence` is 0-100.
//! - `reasons` (optional, dead-code rules): machine-readable reasons for the confidence, e.g.
//!   `["name-elsewhere"]`; absent when the finding has none.
//! - `gated` counts findings with `confidence >= min_confidence` (these make the exit status 1).
//! - Strings are UTF-8; invalid bytes in paths or names are replaced with U+FFFD.
//!
//! New fields may be added without a version bump; removals and meaning changes bump
//! [`JSON_VERSION`]. History:
//!
//! - 2 (0.4.0, additive): top-level `root`; per finding `id` and `fix`.
//! - 2: PM405 `symbol` is `pkg.mod:<name>` (was the module); overload implementations have no
//!   `#N` suffix (see the index docs); PM104 / PM107 values exclude docstrings and test code is
//!   skipped by default (`analysis.include-tests-in-metrics`). Added `end_line`, `value`,
//!   `threshold`, `cycle`, `other`.
//! - 1: initial.

use std::io::Write as _;

use super::buf::{Out, push_u32};
use super::{JSON_VERSION, Options, Report, Summary};
use crate::rules::codes::{DUPLICATE_CODE, IMPORT_CYCLE};
use crate::rules::{Arg, FileId, Finding, SymbolId, reason_names};

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

/// A message argument as a JSON number (`null` for non-numbers). Reals get at most two
/// decimals.
fn push_number(out: &mut Vec<u8>, a: Option<&Arg>) {
    match a {
        Some(&Arg::U32(v)) => push_u32(out, v),
        Some(&Arg::F32(v)) if v.is_finite() => {
            let _ = write!(out, "{}", (f64::from(v) * 100.0).round() / 100.0);
        }
        _ => out.extend_from_slice(b"null"),
    }
}

/// A symbol's stable id as a JSON string (`null` for [`SymbolId::NONE`]).
fn push_symbol(r: &Report<'_>, out: &mut Vec<u8>, s: SymbolId, scratch: &mut Vec<u8>) {
    if s == SymbolId::NONE {
        out.extend_from_slice(b"null");
        return;
    }
    scratch.clear();
    r.ctx().write_symbol_id(s, scratch);
    push_str(out, scratch);
}

/// Symbols as a JSON array.
fn push_symbols(r: &Report<'_>, out: &mut Vec<u8>, args: &[Arg], scratch: &mut Vec<u8>) {
    out.push(b'[');
    let mut first = true;
    for a in args {
        if let Arg::Symbol(s) = *a {
            if !first {
                out.push(b',');
            }
            first = false;
            push_symbol(r, out, s, scratch);
        }
    }
    out.push(b']');
}

/// Rule-specific structured fields (see the module docs): `value` / `threshold` for metric
/// rules (PM1xx, PM2xx), `cycle` for PM301, `other` for PM501.
fn write_details(r: &Report<'_>, f: &Finding, b: &mut Vec<u8>, scratch: &mut Vec<u8>) {
    let args = r.findings().args(f);
    let code = f.rule.code();
    if (100..300).contains(&code) && args.len() >= 3 {
        key(b, "value", false);
        push_number(b, args.get(1));
        key(b, "threshold", false);
        push_number(b, args.get(2));
    } else if f.rule == IMPORT_CYCLE && args.len() >= 3 {
        let extra = r.findings().extra(f);
        key(b, "cycle", false);
        b.push(b'{');
        key(b, "size", true);
        push_number(b, args.first());
        key(b, "members", false);
        push_symbols(r, b, extra, scratch);
        key(b, "witness", false);
        // The message repeats the first module at the end; the witness does not.
        push_symbols(r, b, &args[2..args.len() - 1], scratch);
        b.push(b'}');
    } else if f.rule == DUPLICATE_CODE {
        let extra = r.findings().extra(f);
        if let [Arg::File(file), Arg::U32(line), Arg::U32(end), Arg::Symbol(sym)] = *extra {
            key(b, "other", false);
            b.push(b'{');
            key(b, "path", true);
            scratch.clear();
            r.ctx().write_path(file, scratch);
            push_str(b, scratch);
            key_u32(b, "line", line);
            key_u32(b, "end_line", end);
            key(b, "symbol", false);
            push_symbol(r, b, sym, scratch);
            b.push(b'}');
        }
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
        key(b, "root", false);
        sym.clear();
        r.ctx().write_root_dir(&mut sym);
        if sym.is_empty() {
            b.extend_from_slice(b"null");
        } else {
            push_str(b, &sym);
        }
        key_u32(b, "min_confidence", u32::from(opts.min_confidence));
        b.extend_from_slice(b",\"findings\":[");
    }
    let ids = r.ids();
    let mut fix = String::with_capacity(super::SCRATCH);
    for (i, f) in r.iter().enumerate() {
        r.message(f, &mut msg);
        r.fix(f, &mut fix);
        r.symbol_id(f, &mut sym);
        let def = f.rule.def();
        let b = out.buf();
        b.extend_from_slice(if i == 0 { b"\n  {\"code\":\"PM" } else { b",\n  {\"code\":\"PM" });
        let code = f.rule.code();
        b.extend_from_slice(&[b'0' + (code / 100 % 10) as u8, b'0' + (code / 10 % 10) as u8, b'0' + (code % 10) as u8]);
        b.push(b'"');
        key(b, "id", false);
        b.push(b'"');
        ids.get(i).push(b);
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
        key(b, "end_line", false);
        u32_or_null(b, r.end_line(f));
        key(b, "symbol", false);
        if f.symbol == SymbolId::NONE {
            b.extend_from_slice(b"null");
        } else {
            push_str(b, &sym);
        }
        key(b, "message", false);
        push_str(b, msg.as_bytes());
        write_details(r, f, b, &mut sym);
        if f.reasons != 0 {
            key(b, "reasons", false);
            b.push(b'[');
            for (k, name) in reason_names(f).enumerate() {
                if k > 0 {
                    b.push(b',');
                }
                push_str(b, name.as_bytes());
            }
            b.push(b']');
        }
        key(b, "fix", false);
        push_str(b, fix.as_bytes());
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
