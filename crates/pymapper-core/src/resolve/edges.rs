//! Module-level import edges, aggregated per (importing module, imported module).

use super::imports::{ImportStatus, ModRes, NameRes, res_flags};
use super::modules::ModuleTable;
use crate::index::{FileId, Index, NONE, NodeId};

/// One module-level import edge from a file's package/module node (12 bytes).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ModuleEdge {
    /// The imported package/module node.
    pub dst: NodeId,
    /// Import rows behind the edge.
    pub rows: u32,
    /// [`res_flags`]: the [`res_flags::EDGE_ALL`] flags shared by every row (a `TYPE_ONLY`
    /// edge exists only for type checkers), plus `STAR` if any row is a star import.
    pub flags: u8,
}

/// Where an external edge points.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[repr(u8)]
pub enum ExtKind {
    /// `key` is the `Sym` of the top-level stdlib module name.
    Stdlib = 0,
    /// `key` is the `DistId`.
    ThirdParty = 1,
    /// `key` is the `Sym` of the top-level name.
    Unknown = 2,
}

/// An import edge from a file's module to code that is not loaded (12 bytes).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExtEdge {
    pub key: u32,
    pub rows: u32,
    pub kind: ExtKind,
    /// As [`ModuleEdge::flags`].
    pub flags: u8,
}

/// The imported module node of a row for the module graph: the target module for `Module`
/// rows (a submodule imported by `from pkg import sub`), else the named module (or the deepest
/// resolved prefix of a `MissingModule` row). `NONE` for unresolved rows.
#[inline]
pub(super) fn row_dst(mods: &ModuleTable, p: ModRes, r: NameRes) -> NodeId {
    if r.status == ImportStatus::Module as u8 && r.target != NONE {
        return r.target;
    }
    if p.module == NONE {
        return NONE;
    }
    mods.node(p.module).unwrap_or(NONE)
}

/// The external edge key of a row (the rule the external edges are built with): `(kind,
/// key)` for imports of code that is not loaded, else `None`.
#[inline]
pub(super) fn row_ext(p: ModRes) -> Option<(ExtKind, u32)> {
    match ImportStatus::from_u8(p.status) {
        ImportStatus::Stdlib => Some((ExtKind::Stdlib, p.ext)),
        ImportStatus::ThirdParty => Some((ExtKind::ThirdParty, p.ext)),
        ImportStatus::External if p.ext != NONE => Some((ExtKind::Unknown, p.ext)),
        _ => None,
    }
}

/// Flags of an edge after adding a row with flags `f`: AND of the edge-wide flags, OR of
/// `STAR`; everything else dropped.
fn merge_flags(acc: u8, f: u8) -> u8 {
    (acc & f & res_flags::EDGE_ALL) | ((acc | f) & res_flags::STAR)
}

/// Appends the module edges of file `f` to `out`, sorted by destination node.
pub(super) fn file_edges(ix: &Index, mods: &ModuleTable, p1: &[ModRes], p2: &[NameRes], f: FileId, out: &mut Vec<ModuleEdge>) {
    let base = out.len();
    let src = ix.file_node(f);
    for row in ix.file_facts(f).imports.range() {
        let dst = row_dst(mods, p1[row], p2[row]);
        if dst != NONE && dst != src {
            out.push(ModuleEdge { dst, rows: 1, flags: p2[row].flags });
        }
    }
    out[base..].sort_unstable_by_key(|e| e.dst);
    let mut w = base;
    for i in base..out.len() {
        let e = out[i];
        if w > base && out[w - 1].dst == e.dst {
            let last = &mut out[w - 1];
            last.rows += 1;
            last.flags = merge_flags(last.flags, e.flags);
            continue;
        }
        out[w] = ModuleEdge { flags: merge_flags(e.flags, e.flags), ..e };
        w += 1;
    }
    out.truncate(w);
}

/// Appends the external edges of file `f` to `out`, sorted by (kind, name bytes / dist id).
pub(super) fn file_ext_edges(ix: &Index, p1: &[ModRes], p2: &[NameRes], f: FileId, out: &mut Vec<ExtEdge>) {
    let base = out.len();
    for row in ix.file_facts(f).imports.range() {
        let Some((kind, key)) = row_ext(p1[row]) else { continue };
        out.push(ExtEdge { key, rows: 1, kind, flags: p2[row].flags });
    }
    if out.len() - base > 1 {
        // `Sym` values depend on scheduling: order names by their bytes.
        out[base..].sort_unstable_by(|a, b| {
            a.kind.cmp(&b.kind).then_with(|| match a.kind {
                ExtKind::ThirdParty => a.key.cmp(&b.key),
                _ => ext_name(ix, a.key).cmp(ext_name(ix, b.key)),
            })
        });
    }
    let mut w = base;
    for i in base..out.len() {
        let e = out[i];
        if w > base && out[w - 1].kind == e.kind && out[w - 1].key == e.key {
            let last = &mut out[w - 1];
            last.rows += 1;
            last.flags = merge_flags(last.flags, e.flags);
            continue;
        }
        out[w] = ExtEdge { flags: merge_flags(e.flags, e.flags), ..e };
        w += 1;
    }
    out.truncate(w);
}

/// Bytes of a `Sym`-keyed external name.
#[inline]
pub(super) fn ext_name(ix: &Index, key: u32) -> &[u8] {
    match crate::intern::Sym::from_raw(key) {
        Some(s) => ix.str(s),
        None => b"",
    }
}
