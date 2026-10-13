//! Output of the query subcommands: one buffered writer for both formats.
//!
//! Everything is appended to the core's [`Out`] buffer (64 KiB chunks, graceful on a closed
//! pipe); qualified ids are rendered into one reused scratch buffer. Paths are relative to the
//! working directory in the text and llm modes (`../` outside it, as `check`'s text formats)
//! and to the project root in JSON (absolute outside it); invalid UTF-8 becomes U+FFFD in
//! every mode, as in `check`.

use std::io::Write;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use anyhow::Context;
use pymapper_core::index::{FileId, Index, NodeId};
use pymapper_core::output::json::push_str;
use pymapper_core::output::{JSON_VERSION, Out, push_relative};
use pymapper_core::query::Match;
use pymapper_core::resolve::{Confidence, ExtKind, res_flags};
use pymapper_core::query::ExternalDep;
use pymapper_core::roots::{Origin, Roots};

use super::session::Mode;

/// A writer of query output (see the module docs).
pub struct Writer<'w> {
    pub ix: &'w Index,
    /// Base of displayed paths (the working directory, or the project root for JSON).
    base: &'w Path,
    roots: &'w Roots,
    /// Shorthand for `mode == Mode::Json`.
    pub json: bool,
    /// Shorthand for `mode == Mode::Llm`.
    pub llm: bool,
    out: Out<'w>,
    tmp: Vec<u8>,
}

/// Writes what is buffered; a closed pipe (`pymapper callers x | head`) is not an error.
pub fn finish(w: Writer<'_>) -> anyhow::Result<()> {
    w.out.finish().context("cannot write to stdout")?;
    Ok(())
}

/// Appends `s` with invalid UTF-8 replaced by U+FFFD.
fn push_lossy(out: &mut Vec<u8>, s: &[u8]) {
    for chunk in s.utf8_chunks() {
        out.extend_from_slice(chunk.valid().as_bytes());
        if !chunk.invalid().is_empty() {
            out.extend_from_slice("\u{fffd}".as_bytes());
        }
    }
}

/// Appends `v` in decimal.
fn push_num(out: &mut Vec<u8>, v: u64) {
    let mut digits = [0u8; 20];
    let mut i = digits.len();
    let mut v = v;
    loop {
        i -= 1;
        digits[i] = b'0' + (v % 10) as u8;
        v /= 10;
        if v == 0 {
            break;
        }
    }
    out.extend_from_slice(&digits[i..]);
}

/// Names of the [`res_flags::EDGE_ALL`] flags (and `star`), in bit order.
const FLAG_NAMES: [(u8, &str); 5] = [
    (res_flags::TYPE_ONLY, "type-only"),
    (res_flags::CONDITIONAL, "conditional"),
    (res_flags::TRY, "try"),
    (res_flags::LAZY, "lazy"),
    (res_flags::STAR, "star"),
];

impl<'w> Writer<'w> {
    pub fn new(ix: &'w Index, base: &'w Path, roots: &'w Roots, mode: Mode, sink: &'w mut dyn Write) -> Writer<'w> {
        let (json, llm) = (mode == Mode::Json, mode == Mode::Llm);
        Writer { ix, base, roots, json, llm, out: Out::new(sink), tmp: Vec::with_capacity(256) }
    }

    #[inline]
    pub fn str(&mut self, s: &str) {
        self.out.buf().extend_from_slice(s.as_bytes());
    }

    #[inline]
    pub fn num(&mut self, v: u64) {
        push_num(self.out.buf(), v);
    }

    /// Ends a line and hands full chunks to the sink.
    #[inline]
    pub fn end_line(&mut self) {
        self.out.buf().push(b'\n');
        self.out.maybe_flush();
    }

    /// Path of `p`: JSON relative to the project root (as is outside it), text and llm
    /// relative to the working directory (`../` steps where needed, as `check`).
    pub fn path(&mut self, p: &Path) {
        if self.json {
            let rel = match p.strip_prefix(self.base) {
                Ok(r) if !r.as_os_str().is_empty() => r,
                _ => p,
            };
            push_str(self.out.buf(), rel.as_os_str().as_bytes());
            return;
        }
        let b = self.base.as_os_str().as_bytes();
        let b = if b == b"/" { b } else { b.strip_suffix(b"/").unwrap_or(b) };
        self.tmp.clear();
        push_relative(p.as_os_str().as_bytes(), b, &mut self.tmp);
        push_lossy(self.out.buf(), &self.tmp);
    }

    /// Path of file `f`.
    #[inline]
    pub fn file(&mut self, f: FileId) {
        let p = self.ix.file_path(f);
        self.path(p);
    }

    /// Qualified id of `v` (a JSON string in JSON mode).
    pub fn qid(&mut self, v: NodeId) {
        self.tmp.clear();
        self.ix.qid(v, &mut self.tmp);
        if self.json {
            push_str(self.out.buf(), &self.tmp);
        } else {
            push_lossy(self.out.buf(), &self.tmp);
        }
    }

    /// Short name of `v` for llm rows under a heading that already names its module: a
    /// class or function without the `pkg.mod:` part (`Class.method`), a package or module
    /// whole (`pkg.mod`).
    pub fn short(&mut self, v: NodeId) {
        self.tmp.clear();
        self.ix.qid(v, &mut self.tmp);
        let start = if self.ix.kind(v).is_def() { self.tmp.iter().position(|&b| b == b':').map_or(0, |i| i + 1) } else { 0 };
        push_lossy(self.out.buf(), &self.tmp[start..]);
    }

    /// `path:line` of the definition of `v` (`path` alone for a package or module; nothing
    /// without a file).
    pub fn def_at(&mut self, v: NodeId) {
        let d = SymbolLoc::of(self.ix, v);
        if let Some(f) = d.file {
            self.file(f);
            if d.line > 0 {
                self.str(":");
                self.num(u64::from(d.line));
            }
        }
    }

    /// Concise location prefix `path:line:col: ` (`col` 0: `path:line: `; `line` 0: `path: `).
    pub fn loc(&mut self, f: FileId, line: u32, col: u32) {
        self.file(f);
        if line > 0 {
            self.out.buf().push(b':');
            self.num(u64::from(line));
            if col > 0 {
                self.out.buf().push(b':');
                self.num(u64::from(col));
            }
        }
        self.str(": ");
    }

    /// `path:line:col` (`col` 0: `path:line`; `line` 0: `path`), no trailing separator.
    pub fn loc_plain(&mut self, f: FileId, line: u32, col: u32) {
        self.file(f);
        if line > 0 {
            self.str(":");
            self.num(u64::from(line));
            if col > 0 {
                self.str(":");
                self.num(u64::from(col));
            }
        }
    }

    /// Concise location prefix of the definition of `v` (nothing for a node without a file).
    pub fn node_loc(&mut self, v: NodeId) {
        let s = SymbolLoc::of(self.ix, v);
        if let Some(f) = s.file {
            self.loc(f, s.line, s.col);
        }
    }

    // ---- JSON ----

    /// `{"version":2,"tool":{...},"query":"<name>"` (the object is left open).
    pub fn json_header(&mut self, name: &str) {
        let b = self.out.buf();
        b.extend_from_slice(b"{\"version\":");
        push_num(b, u64::from(JSON_VERSION));
        b.extend_from_slice(b",\"tool\":{\"name\":\"pymapper\",\"version\":");
        push_str(b, env!("CARGO_PKG_VERSION").as_bytes());
        b.extend_from_slice(b"},\"query\":");
        push_str(b, name.as_bytes());
    }

    /// `,"key":` (no comma when `first`).
    #[inline]
    pub fn key(&mut self, k: &str, first: bool) {
        let b = self.out.buf();
        if !first {
            b.push(b',');
        }
        b.push(b'"');
        b.extend_from_slice(k.as_bytes());
        b.extend_from_slice(b"\":");
    }

    /// `,"key":N`.
    #[inline]
    pub fn key_num(&mut self, k: &str, v: u64) {
        self.key(k, false);
        self.num(v);
    }

    /// A JSON string.
    #[inline]
    pub fn json_str(&mut self, s: &str) {
        push_str(self.out.buf(), s.as_bytes());
    }

    /// `,"key":"s"`.
    #[inline]
    pub fn key_str(&mut self, k: &str, s: &str) {
        self.key(k, false);
        push_str(self.out.buf(), s.as_bytes());
    }

    /// `,"key":true|false`.
    #[inline]
    pub fn key_bool(&mut self, k: &str, v: bool) {
        self.key(k, false);
        self.str(if v { "true" } else { "false" });
    }

    /// A number, or `null` for 0.
    pub fn num_or_null(&mut self, v: u32) {
        if v == 0 { self.str("null") } else { self.num(u64::from(v)) }
    }

    /// The fields of a symbol (no braces): `"symbol":"pkg.mod:f","kind":"function",
    /// "path":"pkg/mod.py","line":3,"col":5`; `path`, `line` and `col` are `null` without a
    /// file. `first`: no leading comma.
    pub fn symbol_fields(&mut self, v: NodeId, first: bool) {
        self.key("symbol", first);
        self.qid(v);
        self.key_str("kind", self.ix.kind(v).as_str());
        let s = SymbolLoc::of(self.ix, v);
        self.key("path", false);
        match s.file {
            Some(f) => self.file(f),
            None => self.str("null"),
        }
        self.key("line", false);
        self.num_or_null(s.line);
        self.key("col", false);
        self.num_or_null(s.col);
    }

    /// `{` + [`Writer::symbol_fields`] + `}`.
    pub fn symbol_object(&mut self, v: NodeId) {
        self.str("{");
        self.symbol_fields(v, true);
        self.str("}");
    }

    /// `"key":{"path":..,"line":..,"col":..}` (`col` 0: `null`).
    pub fn key_site(&mut self, k: &str, f: FileId, line: u32, col: u32) {
        self.key(k, false);
        self.str("{");
        self.key("path", true);
        self.file(f);
        self.key("line", false);
        self.num_or_null(line);
        self.key("col", false);
        self.num_or_null(col);
        self.str("}");
    }

    /// Opens the results array: `,"<key>":[`.
    pub fn open_array(&mut self, k: &str) {
        self.key(k, false);
        self.str("[");
    }

    /// Starts array element `i` on its own line.
    #[inline]
    pub fn item(&mut self, i: usize) {
        self.str(if i == 0 { "\n  " } else { ",\n  " });
    }

    /// Closes an array of `n` elements (`]`, on its own line when non-empty).
    pub fn close_array(&mut self, n: usize) {
        self.str(if n == 0 { "]" } else { "\n]" });
        self.out.maybe_flush();
    }

    /// Ends the document: `}` and a newline.
    pub fn json_end(&mut self) {
        self.str("}");
        self.end_line();
    }

    // ---- shared pieces ----

    /// The concise confidence tag ` [likely]` / ` [possible, dispatch]`.
    pub fn conf_tag(&mut self, conf: Confidence, dispatch: bool) {
        self.str(" [");
        self.str(conf.as_str());
        if dispatch {
            self.str(", dispatch");
        }
        self.str("]");
    }

    /// Names of the import edge flags in `flags`: concise `, type-only, lazy`, JSON
    /// `,"flags":["type-only","lazy"]`.
    pub fn import_flags(&mut self, flags: u8) {
        if self.json {
            self.open_array("flags");
            let mut first = true;
            for (bit, name) in FLAG_NAMES {
                if flags & bit != 0 {
                    if !first {
                        self.str(",");
                    }
                    first = false;
                    push_str(self.out.buf(), name.as_bytes());
                }
            }
            self.str("]");
            return;
        }
        for (bit, name) in FLAG_NAMES {
            if flags & bit != 0 {
                self.str(", ");
                self.str(name);
            }
        }
    }

    /// The kind of an import edge for llm output (ISSUES O3/O4, F4): `top-level` when it runs
    /// on import, else the flags that make it deferred or type-only: `inside a function`,
    /// `TYPE_CHECKING`, `try`, `conditional`, plus `star`.
    pub fn import_kind(&mut self, flags: u8) {
        let deferring = res_flags::TYPE_ONLY | res_flags::CONDITIONAL | res_flags::TRY | res_flags::LAZY;
        let mut first = true;
        if flags & deferring == 0 {
            self.str("top-level");
            first = false;
        }
        for (bit, name) in [
            (res_flags::LAZY, "inside a function"),
            (res_flags::TYPE_ONLY, "TYPE_CHECKING"),
            (res_flags::TRY, "try"),
            (res_flags::CONDITIONAL, "conditional"),
            (res_flags::STAR, "star"),
        ] {
            if flags & bit != 0 {
                if !first {
                    self.str(", ");
                }
                first = false;
                self.str(name);
            }
        }
    }

    /// Name of an import of code that is not loaded (top-level module or distribution).
    pub fn external_name(&mut self, e: &ExternalDep) {
        let name: &[u8] = match (e.kind, e.dist()) {
            (ExtKind::ThirdParty, Some(d)) => self.roots.dists.name(d),
            _ => e.name(self.ix).unwrap_or(b"?"),
        };
        if self.json {
            push_str(self.out.buf(), name);
        } else {
            push_lossy(self.out.buf(), name);
        }
    }

    /// The candidates of an ambiguous or unknown symbol: concise one per line
    /// (`path:line:col: qid (kind)`), JSON a document with `error` and `candidates`.
    pub fn candidates(&mut self, name: &str, text: &str, error: &str, matches: &[Match]) {
        if self.json {
            self.json_header(name);
            self.key_str("error", error);
            self.key_str("input", text);
            self.open_array("candidates");
            for (i, m) in matches.iter().enumerate() {
                self.item(i);
                self.str("{");
                self.symbol_fields(m.node, true);
                self.key_str("origin", origin_str(self.ix, m.node));
                self.key_str("match", m.kind.as_str());
                self.str("}");
            }
            self.close_array(matches.len());
            self.json_end();
            return;
        }
        for m in matches {
            self.node_loc(m.node);
            self.qid(m.node);
            self.str(" (");
            self.str(self.ix.kind(m.node).as_str());
            let o = origin_str(self.ix, m.node);
            if o != "first-party" {
                self.str(", ");
                self.str(o);
            }
            self.str(")");
            self.end_line();
        }
    }
}

/// Origin of `v` as a string (`first-party`, `third-party`, `stdlib`).
pub fn origin_str(ix: &Index, v: NodeId) -> &'static str {
    let o = match ix.file(v) {
        Some(f) => ix.file_origin(f),
        None => {
            let d = ix.ancestor_at(v, 0);
            ix.dist_group(d).map_or(Origin::FirstParty, |g| ix.groups()[g as usize].origin)
        }
    };
    match o {
        Origin::FirstParty => "first-party",
        Origin::ThirdParty => "third-party",
        Origin::Stdlib => "stdlib",
    }
}

/// Where a node is defined: file, 1-based line and column of its name. Packages and modules
/// have line 0 and column 0 (concise `path: `, JSON `null`); nodes without a file have none.
#[derive(Clone, Copy)]
pub struct SymbolLoc {
    pub file: Option<FileId>,
    pub line: u32,
    pub col: u32,
}

impl SymbolLoc {
    pub fn of(ix: &Index, v: NodeId) -> SymbolLoc {
        let file = ix.file(v);
        match (ix.kind(v).is_def(), ix.def_row(v)) {
            (true, Some(r)) => {
                let d = &ix.facts().defs;
                SymbolLoc { file, line: d.name_line[r], col: d.name_col[r] + 1 }
            }
            _ => SymbolLoc { file, line: 0, col: 0 },
        }
    }
}
