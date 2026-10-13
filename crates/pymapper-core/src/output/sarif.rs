//! SARIF 2.1.0: one run; `tool.driver.rules` lists the rules that have results (code order,
//! summary as `shortDescription`, explanation as `help`); results reference them by
//! `ruleIndex`.
//!
//! - `level`: error -> `error`, warning -> `warning`, info -> `note`.
//! - Locations: `artifactLocation.uri` is the percent-encoded relative path with `uriBaseId`
//!   `%SRCROOT%` (left undefined: consumers resolve it to the checkout root, which the paths
//!   are relative to); `region.startLine` / `startColumn` when known, and `endLine` when the
//!   span (the definition of a function / class finding, a duplicated block) ends on a later
//!   line. The run declares `columnKind`
//!   `unicodeCodePoints`; columns are byte columns, which are equal for ASCII line prefixes
//!   (see the module docs of `output`).
//! - `logicalLocations[0].fullyQualifiedName`: the symbol's stable qualified id.
//! - `partialFingerprints["pymapperKey/v1"]`: hash of the baseline key (rule + stable id +
//!   stable arguments), so code-scanning services track a finding across runs and edits.
//! - `properties.confidence`: 0-100.

use super::baseline::KeyBuf;
use super::buf::{Out, push_hex64, push_u32};
use super::json::{key, key_u32, push_str};
use super::Report;
use crate::rules::{CODE_SPACE, FileId, Rule, Severity, SymbolId};

const SCHEMA: &str = "https://docs.oasis-open.org/sarif/sarif/v2.1.0/errata01/os/schemas/sarif-schema-2.1.0.json";
const INFO_URI: &str = "https://github.com/pymapper/pymapper";

fn level(s: Severity) -> &'static str {
    match s {
        Severity::Error => "error",
        Severity::Warning => "warning",
        Severity::Info => "note",
    }
}

pub(super) fn write(r: &Report<'_>, out: &mut Out<'_>) {
    // Rules with results, in code order, and code -> ruleIndex.
    let mut index = [u16::MAX; CODE_SPACE];
    for f in r.iter() {
        index[f.rule.code() as usize] = 0;
    }
    let mut rules: Vec<Rule> = Vec::new();
    for (code, slot) in index.iter_mut().enumerate() {
        if *slot == 0 {
            *slot = rules.len() as u16;
            rules.push(Rule(code as u16));
        }
    }

    let b = out.buf();
    b.extend_from_slice(b"{\n  \"$schema\": ");
    push_str(b, SCHEMA.as_bytes());
    b.extend_from_slice(b",\n  \"version\": \"2.1.0\",\n  \"runs\": [\n    {\n      \"tool\": {\n        \"driver\": {");
    key(b, "name", true);
    push_str(b, b"pymapper");
    key(b, "version", false);
    push_str(b, env!("CARGO_PKG_VERSION").as_bytes());
    key(b, "informationUri", false);
    push_str(b, INFO_URI.as_bytes());
    key(b, "rules", false);
    b.push(b'[');
    for (i, rule) in rules.iter().enumerate() {
        let def = rule.def();
        let b = out.buf();
        b.extend_from_slice(if i == 0 { b"\n          {" } else { b",\n          {" });
        key(b, "id", true);
        push_code(b, *rule);
        key(b, "name", false);
        push_str(b, def.name.as_bytes());
        key(b, "shortDescription", false);
        b.extend_from_slice(b"{\"text\":");
        push_str(b, def.summary.as_bytes());
        b.extend_from_slice(b"},\"help\":{\"text\":");
        push_str(b, def.explanation.as_bytes());
        b.extend_from_slice(b",\"markdown\":");
        push_str(b, def.explanation.as_bytes());
        b.extend_from_slice(b"},\"defaultConfiguration\":{\"level\":");
        push_str(b, level(def.severity).as_bytes());
        b.extend_from_slice(b"},\"properties\":{\"category\":");
        push_str(b, def.rule().category().map_or("", |c| c.title()).as_bytes());
        key(b, "cost", false);
        push_str(b, def.cost.as_str().as_bytes());
        b.extend_from_slice(b"}}");
        out.maybe_flush();
    }
    let b = out.buf();
    b.extend_from_slice(if rules.is_empty() { b"]" } else { b"\n        ]" });
    b.extend_from_slice(b"}\n      },");
    b.extend_from_slice(b"\n      \"columnKind\": \"unicodeCodePoints\",\n      \"results\": [");

    let mut msg = String::with_capacity(super::SCRATCH);
    let mut sym = Vec::with_capacity(super::SCRATCH);
    let mut keybuf = KeyBuf::new();
    for (i, f) in r.iter().enumerate() {
        r.message(f, &mut msg);
        r.symbol_id(f, &mut sym);
        let hash = keybuf.hash(r, f);
        let end = r.end_line(f);
        let def = f.rule.def();
        let b = out.buf();
        b.extend_from_slice(if i == 0 { b"\n        {" } else { b",\n        {" });
        key(b, "ruleId", true);
        push_code(b, f.rule);
        key_u32(b, "ruleIndex", u32::from(index[f.rule.code() as usize]));
        key(b, "level", false);
        push_str(b, level(def.severity).as_bytes());
        key(b, "message", false);
        b.extend_from_slice(b"{\"text\":");
        push_str(b, msg.as_bytes());
        b.push(b'}');
        if f.loc.file != FileId::NONE {
            key(b, "locations", false);
            b.extend_from_slice(b"[{\"physicalLocation\":{\"artifactLocation\":{\"uri\":\"");
            push_uri(b, r.path(f));
            b.extend_from_slice(b"\",\"uriBaseId\":\"%SRCROOT%\"}");
            if f.loc.line > 0 {
                b.extend_from_slice(b",\"region\":{\"startLine\":");
                push_u32(b, f.loc.line);
                if f.loc.col > 0 {
                    key_u32(b, "startColumn", f.loc.col);
                }
                if end > f.loc.line {
                    key_u32(b, "endLine", end);
                }
                b.push(b'}');
            }
            b.extend_from_slice(b"}");
            if f.symbol != SymbolId::NONE {
                b.extend_from_slice(b",\"logicalLocations\":[{\"fullyQualifiedName\":");
                push_str(b, &sym);
                b.extend_from_slice(b"}]");
            }
            b.extend_from_slice(b"}]");
        } else if f.symbol != SymbolId::NONE {
            key(b, "locations", false);
            b.extend_from_slice(b"[{\"logicalLocations\":[{\"fullyQualifiedName\":");
            push_str(b, &sym);
            b.extend_from_slice(b"}]}]");
        }
        key(b, "partialFingerprints", false);
        b.extend_from_slice(b"{\"pymapperKey/v1\":\"");
        push_hex64(b, hash);
        b.extend_from_slice(b"\"}");
        key(b, "properties", false);
        b.extend_from_slice(b"{\"confidence\":");
        push_u32(b, u32::from(f.confidence));
        b.extend_from_slice(b"}}");
        out.maybe_flush();
    }
    let b = out.buf();
    b.extend_from_slice(if r.is_empty() { b"]" } else { b"\n      ]" });
    b.extend_from_slice(b"\n    }\n  ]\n}\n");
    out.maybe_flush();
}

/// `"PMxxx"`.
fn push_code(b: &mut Vec<u8>, rule: Rule) {
    b.push(b'"');
    super::buf::push_code(b, rule.code());
    b.push(b'"');
}

/// Percent-encodes a relative path as a URI reference (RFC 3986): unreserved characters and
/// `/` stay, every other byte (including non-ASCII and invalid UTF-8) becomes `%XX`. The
/// result is plain ASCII without `"` or `\`, so it needs no JSON escaping.
pub(crate) fn push_uri(b: &mut Vec<u8>, path: &[u8]) {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    for &c in path {
        if c.is_ascii_alphanumeric() || matches!(c, b'-' | b'.' | b'_' | b'~' | b'/') {
            b.push(c);
        } else {
            b.extend_from_slice(&[b'%', HEX[(c >> 4) as usize], HEX[(c & 0xf) as usize]]);
        }
    }
}
