//! Unused imports across modules (PM405; see the module docs of `deadcode`).

use super::evidence::import_hit;
use super::seeds::FileInfo;
use crate::extract::facts::{evidence_hash, import_flags};
use crate::index::{FileId, Index, NONE, NodeKind};
use crate::intern::Sym;
use crate::reach::{BitSet, FileModes};
use crate::resolve::{Imports, res_flags};
use crate::rules::dead_code::reason;

/// An import that its own file does not use (yet to be checked against other files).
#[derive(Clone, Copy, Debug)]
pub(super) struct ImportCand {
    /// Store-absolute import row.
    pub row: u32,
    pub file: FileId,
    /// The bound name.
    pub name: Sym,
    /// Confidence lost so far.
    pub penalty: u8,
    /// Reasons for the penalties ([`crate::rules::dead_code::reason`] bits).
    pub reasons: u32,
}

/// Confidence lost for each circumstance (see the module docs of `deadcode`).
pub(super) mod penalty {
    pub const DYNAMIC: u8 = 30;
    pub const STAR_IMPORTED: u8 = 40;
    pub const ELSEWHERE: u8 = 40;
    pub const INIT_EXPORT: u8 = 30;
    pub const INIT: u8 = 20;
    pub const INIT_SUBMODULE: u8 = 20;
    pub const CONDITIONAL: u8 = 10;
    /// Under `if TYPE_CHECKING:`: such names are mostly used in quoted annotations, and name
    /// evidence only sees a string that is a plain dotted name (`"Foo"`, not `"Dict[str, Foo]"`).
    pub const TYPE_ONLY: u8 = 50;
}

/// The import rows of included files that their file does not use and no other import
/// resolves through. O(import rows + evidence lookups).
pub(super) fn candidates(ix: &Index, imp: &Imports, info: &FileInfo, modes: &FileModes) -> Vec<ImportCand> {
    let st = &ix.facts().imports;
    let rows = st.len();
    let files = ix.file_count();
    // Rows another import resolves through (re-exports), and files someone star-imports.
    let mut reexported = BitSet::new(rows);
    let mut starred = BitSet::new(files);
    for r in 0..rows as usize {
        if let Some(v) = imp.via(r) {
            reexported.insert(v);
        }
        if st.flags[r] & import_flags::STAR != 0 {
            let m = imp.module_node(r);
            if m != NONE
                && let Some(f) = ix.file(m)
            {
                starred.insert(f);
            }
        }
    }
    let future = ix.strings().get(b"__future__");
    let mut out = Vec::new();
    for f in 0..files {
        if !info.included[f as usize] {
            continue;
        }
        let view = ix.file_view(f);
        let ev = view.evidence();
        let init = ix.kind(ix.file_node(f)) == NodeKind::Package;
        let library = modes.library(f);
        for row in ix.file_facts(f).imports.range() {
            let flags = st.flags[row];
            let rflags = imp.flags(row);
            if flags & (import_flags::STAR | import_flags::TRY_IMPORT_ERROR) != 0 || rflags & res_flags::TRY != 0 {
                continue;
            }
            let module = view.import_module(row);
            if future.is_some() && module.first().copied() == future {
                continue;
            }
            let from = flags & import_flags::FROM != 0;
            let alias = st.alias[row];
            let Some(name) = alias.or(if from { st.name[row] } else { module.first().copied() }) else { continue };
            let mut pen = 0u8;
            let mut why = 0u32;
            if rflags & res_flags::EXPORTED != 0 {
                let idiom = alias.is_some() && alias == if from { st.name[row] } else { module.last().copied() };
                if library || idiom || imp.in_all(f, name) {
                    continue;
                }
                pen += penalty::INIT_EXPORT;
                why |= reason::INIT_REEXPORT;
            }
            if reexported.contains(row as u32) || ev.binary_search(&evidence_hash(ix.str(name))).is_ok() {
                continue;
            }
            if info.dynamic[f as usize] {
                pen += penalty::DYNAMIC;
                why |= reason::MODULE_DYNAMIC_ACCESS;
            }
            if starred.contains(f) && !ix.str(name).starts_with(b"_") {
                pen += penalty::STAR_IMPORTED;
                why |= reason::STAR_IMPORTED;
            }
            if init {
                pen += penalty::INIT;
                why |= reason::PACKAGE_INIT;
                if from && imp.target(row).is_some_and(|t| matches!(ix.kind(t), NodeKind::Package | NodeKind::Module)) {
                    pen += penalty::INIT_SUBMODULE;
                    why |= reason::INIT_SUBMODULE;
                }
            }
            if flags & import_flags::TYPE_CHECKING != 0 {
                pen += penalty::TYPE_ONLY;
                why |= reason::TYPE_CHECKING_IMPORT;
            } else if flags & import_flags::CONDITIONAL != 0 {
                pen += penalty::CONDITIONAL;
                why |= reason::CONDITIONAL_IMPORT;
            }
            out.push(ImportCand { row: row as u32, file: f, name, penalty: pen, reasons: why });
        }
    }
    out
}

/// Confidence and reasons of a candidate after the evidence scan; `None` when the scan found a
/// use.
#[inline]
pub(super) fn confidence(c: &ImportCand, hit: u8) -> Option<(u8, u32)> {
    let mut pen = u32::from(c.penalty);
    let mut why = c.reasons;
    match hit {
        import_hit::USED => return None,
        import_hit::ELSEWHERE => {
            pen += u32::from(penalty::ELSEWHERE);
            why |= reason::NAME_ELSEWHERE;
        }
        _ => why |= reason::NO_REFERENCES,
    }
    Some((100u32.saturating_sub(pen) as u8, why))
}
