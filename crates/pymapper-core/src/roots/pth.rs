//! `.pth` files of a site-packages directory, including editable installs.
//!
//! Follows `site.addpackage`: files are processed in sorted name order (hidden ones skipped),
//! lines starting with `#` and blank lines are skipped, a line that is an existing path
//! (relative to the site directory) is added to the search path, and a line starting with
//! `import` is code. We never run code; instead, for each module named by an `import` line
//! that exists as `<site>/<module>.py`, we scan it for the mappings of the known editable
//! import hooks (best effort):
//!
//! - setuptools (PEP 660): `MAPPING = {'pkg': '/src/pkg'}` and
//!   `NAMESPACES = {'ns': ['/src/ns']}` in `__editable___<dist>_finder.py`;
//! - `editables` (hatchling, pdm): `F.map_module('pkg', '/src/pkg/__init__.py')`.
//!
//! Path-style editables (`/src` lines: pip/setuptools legacy, uv, flit, pdm) need no special
//! handling. Legacy namespace `-nspkg.pth` lines are code without a hook and are ignored; PEP 420
//! namespace handling covers them.

use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use super::dist::{find, read_all};
use super::{Diagnostic, DiagnosticKind, bytes_path, is_identifier};
use crate::config::normalize;

/// Hook modules larger than this are not scanned.
const MAX_HOOK_FILE: u64 = 256 * 1024;

/// One search-path contribution of a `.pth` file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum PthItem {
    /// An existing directory (or zip) appended to the search path.
    Path { path: PathBuf, pth: Box<[u8]> },
    /// A module or package mapped to `path` by an editable import hook. For a package `path` is
    /// its directory, for a single-file module the file. Namespace portions are mapped like
    /// packages (one item per portion).
    Mapped { name: Box<[u8]>, path: PathBuf, pth: Box<[u8]> },
}

/// Reads every `.pth` file of `site` in `site.py` order and appends what they contribute.
pub(super) fn read_site_pth(site: &Path, out: &mut Vec<PthItem>, diags: &mut Vec<Diagnostic>) {
    let Ok(rd) = fs::read_dir(site) else { return };
    let mut names: Vec<Box<[u8]>> = rd
        .flatten()
        .filter_map(|e| {
            let n = e.file_name();
            let b = n.as_bytes();
            (b.ends_with(b".pth") && !b.starts_with(b".")).then(|| b.into())
        })
        .collect();
    names.sort_unstable();
    let mut buf = Vec::new();
    for name in &names {
        let path = site.join(bytes_path(name));
        if let Err(e) = read_all(&path, &mut buf) {
            diags.push(Diagnostic { kind: DiagnosticKind::Pth, path, message: e.to_string() });
            continue;
        }
        parse_pth(site, name, &buf, out);
    }
}

/// Applies the lines of one `.pth` file (`name` in `site`).
pub(super) fn parse_pth(site: &Path, name: &[u8], text: &[u8], out: &mut Vec<PthItem>) {
    let text = text.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(text);
    for line in text.split(|&b| b == b'\n') {
        if line.starts_with(b"#") || line.trim_ascii().is_empty() {
            continue;
        }
        if line.starts_with(b"import ") || line.starts_with(b"import\t") {
            scan_import_line(site, name, line, out);
            continue;
        }
        let line = line.trim_ascii_end();
        let path = normalize(&site.join(bytes_path(line)));
        if path.exists() {
            out.push(PthItem::Path { path, pth: name.into() });
        }
    }
}

/// Looks at the modules imported by an `import a, b; ...` line and scans the ones that live in
/// the site directory for editable-hook mappings.
fn scan_import_line(site: &Path, pth: &[u8], line: &[u8], out: &mut Vec<PthItem>) {
    let stmt = line[b"import".len()..].split(|&b| b == b';').next().unwrap_or(b"");
    let mut buf = Vec::new();
    for part in stmt.split(|&b| b == b',') {
        let module = part.trim_ascii().split(|b| b.is_ascii_whitespace()).next().unwrap_or(b"");
        if !is_identifier(module) {
            continue;
        }
        let mut file = site.join(bytes_path(module));
        file.as_mut_os_string().push(".py");
        match fs::metadata(&file) {
            Ok(m) if m.is_file() && m.len() <= MAX_HOOK_FILE => {}
            _ => continue,
        }
        if read_all(&file, &mut buf).is_ok() {
            for (name, path) in hook_mappings(&buf) {
                out.push(PthItem::Mapped { name: name.into(), path: mapped_path(&path), pth: pth.into() });
            }
        }
    }
}

/// A mapped `.../pkg/__init__.py` is the package directory; other paths stay as they are.
fn mapped_path(p: &[u8]) -> PathBuf {
    let p = p.strip_suffix(b"/__init__.py").unwrap_or(p);
    normalize(bytes_path(p))
}

/// `(module name, path)` pairs from the source of an editable import hook.
pub(super) fn hook_mappings(src: &[u8]) -> Vec<(Vec<u8>, Vec<u8>)> {
    let mut out = Vec::new();
    // setuptools: MAPPING (str -> str) and NAMESPACES (str -> list[str]).
    for var in [&b"MAPPING"[..], b"NAMESPACES"] {
        if let Some(at) = assignment(src, var) {
            let mut lit = Lit { s: src, i: at };
            lit.dict(|k, v| out.push((k.to_vec(), v.to_vec())));
        }
    }
    // editables: map_module('name', 'path').
    let mut from = 0;
    while let Some(i) = find(&src[from..], b"map_module(") {
        let mut lit = Lit { s: src, i: from + i + b"map_module(".len() };
        from = lit.i;
        let Some(name) = lit.string() else { continue };
        lit.ws();
        if !lit.eat(b',') {
            continue;
        }
        if let Some(path) = lit.string()
            && is_dotted(&name)
        {
            out.push((name, path));
        }
    }
    out.retain(|(name, path)| is_dotted(name) && path.first() == Some(&b'/'));
    out
}

fn is_dotted(name: &[u8]) -> bool {
    !name.is_empty() && name.split(|&b| b == b'.').all(is_identifier)
}

/// Offset just after `VAR [: annotation] =` at the start of a line, if present.
fn assignment(src: &[u8], var: &[u8]) -> Option<usize> {
    let mut from = 0;
    while let Some(i) = find(&src[from..], var) {
        let at = from + i;
        from = at + var.len();
        if at > 0 && src[at - 1] != b'\n' {
            continue;
        }
        let rest = &src[from..];
        let line_end = rest.iter().position(|&b| b == b'\n').unwrap_or(rest.len());
        let line = &rest[..line_end];
        // `VAR =` or `VAR: dict[...] =`, but not `VAR ==` / `VARIABLE =`.
        if !matches!(line.first(), Some(b' ' | b'\t' | b':' | b'=')) {
            continue;
        }
        if let Some(eq) = line.iter().position(|&b| b == b'=')
            && line.get(eq + 1) != Some(&b'=')
        {
            return Some(from + eq + 1);
        }
    }
    None
}

/// A tiny reader for Python literals: strings, `{str: str | [str, ...]}` dicts.
struct Lit<'a> {
    s: &'a [u8],
    i: usize,
}

impl Lit<'_> {
    /// Skips whitespace, comments and line continuations.
    fn ws(&mut self) {
        while self.i < self.s.len() {
            match self.s[self.i] {
                b' ' | b'\t' | b'\n' | b'\r' | b'\\' => self.i += 1,
                b'#' => {
                    while self.i < self.s.len() && self.s[self.i] != b'\n' {
                        self.i += 1;
                    }
                }
                _ => break,
            }
        }
    }

    fn eat(&mut self, c: u8) -> bool {
        self.ws();
        if self.s.get(self.i) == Some(&c) {
            self.i += 1;
            true
        } else {
            false
        }
    }

    /// A single- or double-quoted string with an optional `r`/`u` prefix.
    fn string(&mut self) -> Option<Vec<u8>> {
        self.ws();
        let mut raw = false;
        while let Some(&c) = self.s.get(self.i) {
            match c {
                b'r' | b'R' => raw = true,
                b'u' | b'U' => {}
                _ => break,
            }
            self.i += 1;
        }
        let q = *self.s.get(self.i).filter(|&&c| c == b'\'' || c == b'"')?;
        self.i += 1;
        let mut out = Vec::new();
        while let Some(&c) = self.s.get(self.i) {
            self.i += 1;
            if c == q {
                return Some(out);
            }
            if c == b'\n' {
                return None;
            }
            if c == b'\\' && !raw {
                let e = *self.s.get(self.i)?;
                self.i += 1;
                match e {
                    b'n' => out.push(b'\n'),
                    b't' => out.push(b'\t'),
                    b'\\' | b'\'' | b'"' => out.push(e),
                    _ => out.extend_from_slice(&[b'\\', e]),
                }
                continue;
            }
            out.push(c);
        }
        None
    }

    /// `{key: value, ...}` where a value is a string or a list/tuple of strings; calls `f` once
    /// per (key, string). Stops quietly at anything else.
    fn dict(&mut self, mut f: impl FnMut(&[u8], &[u8])) {
        if !self.eat(b'{') {
            return;
        }
        loop {
            if self.eat(b'}') {
                return;
            }
            let Some(key) = self.string() else { return };
            if !self.eat(b':') {
                return;
            }
            self.ws();
            match self.s.get(self.i) {
                Some(b'[' | b'(') => {
                    let close = if self.s[self.i] == b'[' { b']' } else { b')' };
                    self.i += 1;
                    loop {
                        if self.eat(close) {
                            break;
                        }
                        let Some(v) = self.string() else { return };
                        f(&key, &v);
                        if !self.eat(b',') {
                            if !self.eat(close) {
                                return;
                            }
                            break;
                        }
                    }
                }
                _ => {
                    let Some(v) = self.string() else { return };
                    f(&key, &v);
                }
            }
            if !self.eat(b',') {
                self.eat(b'}');
                return;
            }
        }
    }
}
