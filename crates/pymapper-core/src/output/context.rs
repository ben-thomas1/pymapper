//! What output needs from the rest of the program: display paths, stable symbol ids and the
//! message argument resolver. [`IndexContext`] implements it over the global [`Index`].
//!
//! # Paths
//!
//! Two kinds of path are rendered:
//!
//! - **display paths** ([`Context::write_path`], `Arg::File` in messages): relative to the
//!   display base. For the human, llm, concise and github formats the CLI sets the base to the
//!   working directory ([`IndexContext::relative_to`]), so paths are clickable from the terminal
//!   (`sample_python/app.py`, `../lib/x.py`); json and sarif keep the project root.
//! - **root paths** ([`Context::write_root_path`]): always relative to the project root. Used
//!   for identity (baseline keys, finding ids), so they do not depend on the working directory.

use std::cell::RefCell;
use std::fmt;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use crate::config::Contract;
use crate::index::Index;
use crate::intern::Sym;
use crate::rules::{ArgResolver, FileId, SymbolId};

/// Names for output. Implemented over the index by [`IndexContext`]; tests and the GUI can
/// provide their own.
pub trait Context: ArgResolver {
    /// Appends the display path of `file` (relative to the project root where possible).
    /// Called once per file with findings per report.
    fn write_path(&self, file: FileId, out: &mut Vec<u8>);
    /// Appends the path of `file` relative to the project root (absolute outside it), whatever
    /// the display base. Used for baseline keys and finding ids.
    fn write_root_path(&self, file: FileId, out: &mut Vec<u8>) {
        self.write_path(file, out);
    }
    /// Appends the stable qualified id of `sym` (`pkg.mod:Class.method`), as used by baselines.
    fn write_symbol_id(&self, sym: SymbolId, out: &mut Vec<u8>);
    /// Last line of `sym`'s definition when it is a class or function, else 0 (for the JSON
    /// `end_line` of findings whose analysis did not record one).
    fn def_end_line(&self, _sym: SymbolId) -> u32 {
        0
    }
}

/// Writes bytes to a `fmt::Write`, replacing invalid UTF-8 with U+FFFD.
pub(crate) fn write_lossy(bytes: &[u8], out: &mut dyn fmt::Write) -> fmt::Result {
    for chunk in bytes.utf8_chunks() {
        out.write_str(chunk.valid())?;
        if !chunk.invalid().is_empty() {
            out.write_char(char::REPLACEMENT_CHARACTER)?;
        }
    }
    Ok(())
}

/// [`Context`] over the global index: [`FileId`]s are index file ids, [`SymbolId`]s index node
/// ids, `Name` arguments raw [`Sym`] values of the index interner, `Contract` arguments indices
/// into `contracts`. Unknown ids render as `?`.
pub struct IndexContext<'a> {
    index: &'a Index,
    /// Root path bytes without a trailing `/` (empty: no stripping).
    root: &'a [u8],
    /// Display base (absolute, without a trailing `/`); `None` = the root.
    base: Option<Vec<u8>>,
    contracts: &'a [Contract],
    /// Scratch for ids rendered through `fmt::Write` (output is single-threaded).
    scratch: RefCell<Vec<u8>>,
}

impl<'a> IndexContext<'a> {
    /// Paths under `root` are shown relative to it.
    pub fn new(index: &'a Index, root: &'a Path, contracts: &'a [Contract]) -> IndexContext<'a> {
        let rb = root.as_os_str().as_bytes();
        let root = if rb == b"/" { rb } else { rb.strip_suffix(b"/").unwrap_or(rb) };
        IndexContext { index, root, base: None, contracts, scratch: RefCell::new(Vec::new()) }
    }

    /// Shows display paths relative to `base` (absolute; normally the working directory)
    /// instead of the root: `../` steps where the file is not below it, the absolute path when
    /// the two share nothing but `/`. Root paths are unchanged.
    pub fn relative_to(mut self, base: &Path) -> IndexContext<'a> {
        let b = base.as_os_str().as_bytes();
        let b = if b == b"/" { b } else { b.strip_suffix(b"/").unwrap_or(b) };
        if b != self.root {
            self.base = Some(b.to_vec());
        }
        self
    }

    fn root_path(&self, file: FileId) -> Option<&'a [u8]> {
        if file.0 >= self.index.file_count() {
            return None;
        }
        let p = self.index.file_path(file.0).as_os_str().as_bytes();
        if self.root.is_empty() {
            return Some(p);
        }
        let rel = match p.strip_prefix(self.root) {
            Some(rest) if self.root == b"/" => Some(rest),
            Some([b'/', rest @ ..]) if !rest.is_empty() => Some(rest),
            _ => None,
        };
        Some(rel.unwrap_or(p))
    }

    /// Appends the display path of `file`; false for an unknown file.
    fn push_display(&self, file: FileId, out: &mut Vec<u8>) -> bool {
        match &self.base {
            None => match self.root_path(file) {
                Some(p) => {
                    out.extend_from_slice(p);
                    true
                }
                None => false,
            },
            Some(base) => {
                if file.0 >= self.index.file_count() {
                    return false;
                }
                push_relative(self.index.file_path(file.0).as_os_str().as_bytes(), base, out);
                true
            }
        }
    }

    fn valid_node(&self, sym: SymbolId) -> bool {
        sym.0 < self.index.node_count()
    }
}

impl Context for IndexContext<'_> {
    fn write_path(&self, file: FileId, out: &mut Vec<u8>) {
        if !self.push_display(file, out) {
            out.push(b'?');
        }
    }

    fn write_root_path(&self, file: FileId, out: &mut Vec<u8>) {
        match self.root_path(file) {
            Some(p) => out.extend_from_slice(p),
            None => out.push(b'?'),
        }
    }

    fn write_symbol_id(&self, sym: SymbolId, out: &mut Vec<u8>) {
        if self.valid_node(sym) {
            self.index.qid(sym.0, out);
        } else {
            out.push(b'?');
        }
    }

    fn def_end_line(&self, sym: SymbolId) -> u32 {
        if !self.valid_node(sym) || !self.index.kind(sym.0).is_def() {
            return 0;
        }
        self.index.def_row(sym.0).map_or(0, |row| self.index.facts().defs.end_line[row])
    }
}

impl ArgResolver for IndexContext<'_> {
    fn write_symbol(&self, id: SymbolId, out: &mut dyn fmt::Write) -> fmt::Result {
        let mut buf = self.scratch.borrow_mut();
        buf.clear();
        self.write_symbol_id(id, &mut buf);
        write_lossy(&buf, out)
    }

    fn write_name(&self, id: u32, out: &mut dyn fmt::Write) -> fmt::Result {
        match Sym::from_raw(id) {
            Some(s) if s.index() < self.index.strings().len() => write_lossy(self.index.str(s), out),
            _ => out.write_str("?"),
        }
    }

    fn write_file(&self, id: FileId, out: &mut dyn fmt::Write) -> fmt::Result {
        let mut buf = self.scratch.borrow_mut();
        buf.clear();
        if self.push_display(id, &mut buf) { write_lossy(&buf, out) } else { out.write_str("?") }
    }

    fn write_contract(&self, index: u32, out: &mut dyn fmt::Write) -> fmt::Result {
        match self.contracts.get(index as usize) {
            Some(c) => out.write_str(&c.name),
            None => out.write_str("?"),
        }
    }
}

/// Appends `path` relative to `base` (both absolute, `base` without a trailing `/`): the part
/// below `base`, else `../` steps up to the deepest shared directory and the rest, else (only
/// `/` shared) `path` itself. `base` itself shows as `.`.
pub fn push_relative(path: &[u8], base: &[u8], out: &mut Vec<u8>) {
    let comps = |p: &[u8]| -> Vec<(usize, usize)> {
        let mut v = Vec::new();
        let mut start = 0;
        for (i, &b) in p.iter().enumerate() {
            if b == b'/' {
                if i > start {
                    v.push((start, i));
                }
                start = i + 1;
            }
        }
        if p.len() > start {
            v.push((start, p.len()));
        }
        v
    };
    let pc = comps(path);
    let bc = comps(base);
    let mut common = 0;
    while common < pc.len() && common < bc.len() && path[pc[common].0..pc[common].1] == base[bc[common].0..bc[common].1] {
        common += 1;
    }
    if common == 0 {
        out.extend_from_slice(path);
        return;
    }
    let start = out.len();
    for _ in common..bc.len() {
        out.extend_from_slice(b"../");
    }
    for &(s, e) in &pc[common..] {
        out.extend_from_slice(&path[s..e]);
        out.push(b'/');
    }
    if out.len() == start {
        out.push(b'.');
    } else {
        out.pop();
    }
}
