//! What output needs from the rest of the program: display paths, stable symbol ids and the
//! message argument resolver. [`IndexContext`] implements it over the global [`Index`].

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
    contracts: &'a [Contract],
    /// Scratch for ids rendered through `fmt::Write` (output is single-threaded).
    scratch: RefCell<Vec<u8>>,
}

impl<'a> IndexContext<'a> {
    /// Paths under `root` are shown relative to it.
    pub fn new(index: &'a Index, root: &'a Path, contracts: &'a [Contract]) -> IndexContext<'a> {
        let rb = root.as_os_str().as_bytes();
        let root = if rb == b"/" { rb } else { rb.strip_suffix(b"/").unwrap_or(rb) };
        IndexContext { index, root, contracts, scratch: RefCell::new(Vec::new()) }
    }

    fn display_path(&self, file: FileId) -> Option<&'a [u8]> {
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

    fn valid_node(&self, sym: SymbolId) -> bool {
        sym.0 < self.index.node_count()
    }
}

impl Context for IndexContext<'_> {
    fn write_path(&self, file: FileId, out: &mut Vec<u8>) {
        match self.display_path(file) {
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
        match self.display_path(id) {
            Some(p) => write_lossy(p, out),
            None => out.write_str("?"),
        }
    }

    fn write_contract(&self, index: u32, out: &mut dyn fmt::Write) -> fmt::Result {
        match self.contracts.get(index as usize) {
            Some(c) => out.write_str(&c.name),
            None => out.write_str("?"),
        }
    }
}
