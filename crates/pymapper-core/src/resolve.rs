//! Import and call resolution (design: Semantics). Owned by the resolve step.
//!
//! # Imports
//!
//! [`resolve_imports`] resolves every import row of an [`Index`] and builds the module-level
//! import graph. Everything runs in parallel per file (rayon) against read-only tables:
//!
//! ```text
//!   Index ──> ModuleTable    trie of dotted names: (parent, segment Sym) -> ModId, one target
//!         │                  node per name (implementation > stub > namespace > shadowed)
//!         ├─> Namespaces     per file: module-level names sorted by Sym (def > import >
//!         │                  assignment), `__all__` membership, star imports
//!         ├─> phase 1        per row: module part (absolute / relative) -> ModId, or
//!         │                  attribution (stdlib / distribution / external) via Attribution
//!         ├─> phase 2        per row: imported name -> def / module / variable, following
//!         │                  re-exports (`from .x import y` in `__init__`) and star imports
//!         └─> edges          per file: module edges (dst, rows, flags) + external edges
//! ```
//!
//! Lookups are hash lookups on `u64` keys (trie) and binary searches over a file's namespace
//! run: no string is built or hashed per import, nothing is allocated per row beyond the
//! output columns (24 bytes per import row). Re-export chains are followed up to 32 hops; star
//! imports depth-first with a per-lookup visited set of (module, name) pairs, capped at 512
//! (no exponential blow-up on star cycles).
//!
//! ## Semantics
//!
//! - `import a.b.c` targets module `a.b.c` (and binds `a`; `import a.b as x` binds `a.b`).
//! - `from m import n`: a module-level binding `n` of `m` (implementation, then its stub),
//!   else submodule `m.n`, else `m`'s star imports (names in the source's `__all__`, else
//!   public names; submodules only through `__all__`), else `__getattr__` (dynamic), else a
//!   missing name. A binding that is itself an import is followed (re-export).
//! - Relative imports start at the file's package (`__init__` files are their own package).
//! - Unresolved: a regular package prefix makes it [`ImportStatus::MissingModule`];
//!   otherwise the top-level name is attributed to the stdlib, a distribution, or external.
//! - Flags per row ([`res_flags`]): `TYPE_CHECKING` imports are type-only, conditional and
//!   `try`/`except ImportError` imports are flagged, function-level imports are lazy, module
//!   re-exports are marked (`__all__`, `import x as x`, public names of an `__init__` without
//!   `__all__`).
//! - Module edges go from a file's node to the imported module (to the submodule for
//!   `from pkg import sub`, to the named module for symbols, even when re-exported from
//!   elsewhere: that is what runs at import time). Self edges are dropped. An edge keeps
//!   `TYPE_ONLY` / `CONDITIONAL` / `TRY` / `LAZY` only if all its rows have it.
//!
//! Determinism: every output depends only on the index contents and node ids, never on thread
//! scheduling or `Sym` values.

mod attrib;
mod edges;
mod imports;
mod modules;
mod namespace;

#[cfg(test)]
mod tests;

pub use attrib::{Attributed, Attribution, is_builtin_stdlib};
pub use edges::{ExtEdge, ExtKind, ModuleEdge};
pub use imports::{ImportStatus, res_flags};
pub use modules::{ModId, ModKind, ModuleTable};

use std::time::Instant;

use imports::{Ctx, Found, ModRes, NameRes, Names, Scratch};
use namespace::{Namespaces, kind, par_concat, par_concat_with};

use crate::index::{FileId, Index, NONE, NodeId};
use crate::intern::Sym;
use crate::roots::DistId;
use crate::timing;

/// Counters and phase times of [`resolve_imports`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ImportStats {
    pub rows: u64,
    /// Rows per [`ImportStatus`] (index = status as `u8`).
    pub by_status: [u64; 10],
    pub module_edges: u64,
    pub ext_edges: u64,
    pub modules: u64,
    pub build: std::time::Duration,
    pub resolve: std::time::Duration,
    pub edges: std::time::Duration,
}

/// Resolved imports of an index and its module-level import graph (see the module docs).
pub struct Imports {
    modules: ModuleTable,
    ns: Namespaces,
    /// Per import row (store-absolute rows of `Index::facts().imports`).
    p1: Vec<ModRes>,
    p2: Vec<NameRes>,
    edges: Vec<ModuleEdge>,
    edge_start: Vec<u32>,
    ext_edges: Vec<ExtEdge>,
    ext_start: Vec<u32>,
    getattr: Option<Sym>,
    stats: ImportStats,
}

/// What a module-level name of a file refers to ([`Imports::global`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Global {
    /// A class/function node (defined in the file or imported).
    Def(NodeId),
    /// A package/module node.
    Module(NodeId),
    /// An assignment in the module with this node.
    Variable(NodeId),
    /// An import that did not resolve to loaded code (its row).
    Unresolved(u32),
    /// Bound through a star import of unresolved code or dynamically.
    Dynamic,
}

/// Resolves every import of `ix` (see the module docs). `attr` attributes imports of code that
/// is not loaded. Runs on the current rayon pool. Phases are recorded with [`timing`] as
/// `resolve.modules`, `resolve.imports` and `resolve.edges`.
pub fn resolve_imports(ix: &Index, attr: &Attribution) -> Imports {
    let t0 = Instant::now();
    let (modules, ns) = timing::time("resolve.modules", || {
        rayon::join(|| ModuleTable::build(ix), || Namespaces::build(ix))
    });
    let t1 = Instant::now();
    let getattr = ix.strings().get(b"__getattr__");
    let files = ix.file_count() as usize;
    let cx = Ctx { ix, mods: &modules, ns: &ns, getattr };
    let (p1, p2) = timing::time("resolve.imports", || {
        let (p1, _) = par_concat(files, |f, out| {
            for row in ix.file_facts(f).imports.range() {
                out.push(cx.module_part(attr, f, row));
            }
        });
        let names = Names { cx: &cx, p1: &p1 };
        let (p2, _) = par_concat_with(files, |sc: &mut Scratch, f, out| {
            for row in ix.file_facts(f).imports.range() {
                out.push(names.row(f, row, sc));
            }
        });
        (p1, p2)
    });
    // Rows of file f are its run of the store, files in FileId order from row 0.
    debug_assert!((0..ix.file_count()).all(|f| ix.file_facts(f).imports.range().end <= p1.len()));
    debug_assert_eq!(p1.len(), ix.facts().imports.len() as usize);
    let t2 = Instant::now();
    let ((edges, edge_start), (ext_edges, ext_start)) = timing::time("resolve.edges", || {
        rayon::join(
            || par_concat(files, |f, out| edges::file_edges(ix, &modules, &p1, &p2, f, out)),
            || par_concat(files, |f, out| edges::file_ext_edges(ix, &p1, &p2, f, out)),
        )
    });
    let t3 = Instant::now();
    let mut stats = ImportStats {
        rows: p2.len() as u64,
        module_edges: edges.len() as u64,
        ext_edges: ext_edges.len() as u64,
        modules: u64::from(modules.len()),
        build: t1 - t0,
        resolve: t2 - t1,
        edges: t3 - t2,
        ..ImportStats::default()
    };
    for r in &p2 {
        stats.by_status[r.status as usize] += 1;
    }
    Imports { modules, ns, p1, p2, edges, edge_start, ext_edges, ext_start, getattr, stats }
}

impl Imports {
    #[inline]
    fn names<'a>(&'a self, cx: &'a Ctx<'a>) -> Names<'a> {
        Names { cx, p1: &self.p1 }
    }

    #[inline]
    fn ctx<'a>(&'a self, ix: &'a Index) -> Ctx<'a> {
        Ctx { ix, mods: &self.modules, ns: &self.ns, getattr: self.getattr }
    }

    /// The module table (dotted names of the index).
    #[inline]
    pub fn modules(&self) -> &ModuleTable {
        &self.modules
    }

    #[inline]
    pub fn stats(&self) -> &ImportStats {
        &self.stats
    }

    /// Number of import rows (= `Index::facts().imports.len()`).
    #[inline]
    pub fn len(&self) -> usize {
        self.p2.len()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.p2.is_empty()
    }

    /// Final status of import `row` (store-absolute).
    #[inline]
    pub fn status(&self, row: usize) -> ImportStatus {
        ImportStatus::from_u8(self.p2[row].status)
    }

    /// The module part of `row`: the named module, or the deepest resolved prefix of a
    /// [`ImportStatus::MissingModule`] row.
    #[inline]
    pub fn module(&self, row: usize) -> Option<ModId> {
        let m = self.p1[row].module;
        (m != NONE).then_some(m)
    }

    /// Target node of `row`: a package/module node (`Module`, `Variable`) or a class/function
    /// node (`Symbol`).
    #[inline]
    pub fn target(&self, row: usize) -> Option<NodeId> {
        let t = self.p2[row].target;
        (t != NONE).then_some(t)
    }

    /// The distribution of a [`ImportStatus::ThirdParty`] row (also when re-exported).
    #[inline]
    pub fn dist(&self, row: usize) -> Option<DistId> {
        let r = self.ext_row(row)?;
        (self.p1[r].status == ImportStatus::ThirdParty as u8).then_some(DistId(self.p1[r].ext))
    }

    /// The top-level name of a [`ImportStatus::Stdlib`] / [`ImportStatus::External`] row
    /// (also when re-exported).
    #[inline]
    pub fn ext_name<'a>(&self, ix: &'a Index, row: usize) -> Option<&'a [u8]> {
        let r = self.ext_row(row)?;
        let p = self.p1[r];
        let named = p.status == ImportStatus::Stdlib as u8 || p.status == ImportStatus::External as u8;
        (named && p.ext != NONE).then(|| edges::ext_name(ix, p.ext))
    }

    /// The row whose module part carries the attribution of external `row`: itself, or the
    /// last hop of its re-export chain.
    fn ext_row(&self, row: usize) -> Option<usize> {
        if !self.status(row).is_external() {
            return None;
        }
        let mut cur = row;
        // Bounded like the resolution (each hop is a module-level import row).
        for _ in 0..64 {
            if ImportStatus::from_u8(self.p1[cur].status).is_external() {
                return Some(cur);
            }
            let via = self.p2[cur].via;
            if via == NONE {
                return None;
            }
            cur = via as usize;
        }
        None
    }

    /// First re-export hop of `row`: the import row of the named module the name was found
    /// through (set with [`res_flags::VIA_REEXPORT`]).
    #[inline]
    pub fn via(&self, row: usize) -> Option<u32> {
        let v = self.p2[row].via;
        (v != NONE).then_some(v)
    }

    /// [`res_flags`] of `row`.
    #[inline]
    pub fn flags(&self, row: usize) -> u8 {
        self.p2[row].flags
    }

    /// Module edges of file `f` (from `Index::file_node(f)`), sorted by destination node.
    #[inline]
    pub fn module_edges(&self, f: FileId) -> &[ModuleEdge] {
        &self.edges[self.edge_start[f as usize] as usize..self.edge_start[f as usize + 1] as usize]
    }

    /// External edges of file `f`, sorted by kind then name / distribution.
    #[inline]
    pub fn ext_edges(&self, f: FileId) -> &[ExtEdge] {
        &self.ext_edges[self.ext_start[f as usize] as usize..self.ext_start[f as usize + 1] as usize]
    }

    /// Every module edge as `(file, edge)`, in FileId order.
    pub fn all_module_edges(&self) -> impl Iterator<Item = (FileId, &ModuleEdge)> + '_ {
        self.edge_start.windows(2).enumerate().flat_map(move |(f, w)| {
            self.edges[w[0] as usize..w[1] as usize].iter().map(move |e| (f as FileId, e))
        })
    }

    /// The module-level binding of `name` in file `f`, including names bound through star
    /// imports. Imports and `X = Y` aliases are followed to what they bind. `None` if the
    /// module binds no such name.
    pub fn global(&self, ix: &Index, f: FileId, name: Sym) -> Option<Global> {
        match self.ns.get(f, name) {
            Some(e) if e.base() == kind::DEF => return Some(Global::Def(ix.def_node(f, e.r))),
            Some(e) if e.base() == kind::VAR => {
                // `X = Y` / `X = a.b` aliases of a class, function or module.
                let cx = self.ctx(ix);
                let aliased = self.modules.file_mod(f).and_then(|m| {
                    self.names(&cx).alias(f, e.r as usize, m, 0, &mut Scratch::default())
                });
                return Some(match aliased {
                    Some(Found::Hit(r)) => self.to_global(r, NONE),
                    _ => Global::Variable(ix.file_node(f)),
                });
            }
            Some(e) if e.base() == kind::IMPORT => return Some(self.bound_by(ix, e.r as usize)),
            _ => {}
        }
        let cx = self.ctx(ix);
        self.names(&cx).through_stars(f, name).map(|r| self.to_global(r, NONE))
    }

    /// Attribute `name` of module `m`, with `from m import name` semantics (bindings,
    /// re-exports, submodules, star imports).
    pub fn attribute(&self, ix: &Index, m: ModId, name: Sym) -> Option<Global> {
        let cx = self.ctx(ix);
        self.names(&cx).attribute(m, name).map(|r| self.to_global(r, NONE))
    }

    /// What import `row` binds its name to (`import a.b` binds the top-level package `a`).
    pub fn bound_by(&self, ix: &Index, row: usize) -> Global {
        let st = ix.facts();
        let plain = st.imports.flags[row] & crate::extract::facts::import_flags::FROM == 0;
        let m = self.p1[row].module;
        if plain && st.imports.alias[row].is_none() && m != NONE {
            // Also when the rest of the dotted name is missing: `a` itself is bound.
            if let Some(v) = self.modules.node(self.modules.top(m)) {
                return Global::Module(v);
            }
        }
        self.to_global(self.p2[row], row as u32)
    }

    fn to_global(&self, r: NameRes, row: u32) -> Global {
        match ImportStatus::from_u8(r.status) {
            ImportStatus::Symbol if r.target != NONE => Global::Def(r.target),
            ImportStatus::Module if r.target != NONE => Global::Module(r.target),
            ImportStatus::Variable if r.target != NONE => Global::Variable(r.target),
            ImportStatus::Dynamic => Global::Dynamic,
            _ if row != NONE => Global::Unresolved(row),
            _ if r.via != NONE => Global::Unresolved(r.via),
            _ => Global::Dynamic,
        }
    }

    /// Whether module-level name `name` of file `f` is listed in its `__all__`.
    pub fn in_all(&self, f: FileId, name: Sym) -> bool {
        self.ns.get(f, name).is_some_and(|e| e.in_all())
    }

    /// Heap bytes (capacity) of every table.
    pub fn heap_bytes(&self) -> usize {
        self.modules.heap_bytes()
            + self.ns.heap_bytes()
            + self.p1.capacity() * size_of::<ModRes>()
            + self.p2.capacity() * size_of::<NameRes>()
            + self.edges.capacity() * size_of::<ModuleEdge>()
            + self.ext_edges.capacity() * size_of::<ExtEdge>()
            + (self.edge_start.capacity() + self.ext_start.capacity()) * 4
    }
}
