//! Import resolution: module part (phase 1), then imported names through re-exports and star
//! imports (phase 2). Both phases run in parallel per file against read-only tables.

use super::attrib::{Attributed, Attribution};
use super::modules::{ModId, ModuleTable};
use super::namespace::{Entry, Namespaces, kind};
use crate::extract::facts::{file_flags, import_flags};
use crate::index::{FileId, Index, NONE, NodeId, NodeKind};
use crate::intern::Sym;

/// How an import row resolved.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum ImportStatus {
    /// The target is a package/module node (`import a.b`, `from a import submodule`,
    /// `from a import *`).
    Module = 0,
    /// The target is a class/function node.
    Symbol = 1,
    /// The name is bound by an assignment in a module; the target is that module's node.
    Variable = 2,
    /// The module resolved but the name is not statically known: module `__getattr__`, a star
    /// import from unresolved code, or a re-export chain too long or cyclic.
    Dynamic = 3,
    /// The module resolved but binds no such name.
    MissingName = 4,
    /// A prefix resolved to a regular package or module, the rest did not (an extension
    /// module, a missing file); [`Imports::module`] is the deepest prefix.
    MissingModule = 5,
    /// A relative import above the top-level package.
    BeyondTop = 6,
    /// Not loaded; the top-level name is a stdlib module ([`Imports::ext`] is its `Sym`).
    Stdlib = 7,
    /// Not loaded; provided by a third-party distribution ([`Imports::ext`] is the `DistId`).
    ThirdParty = 8,
    /// Not loaded and of unknown origin ([`Imports::ext`] is the top-level name's `Sym`).
    External = 9,
}

impl ImportStatus {
    #[inline]
    pub(super) fn from_u8(v: u8) -> ImportStatus {
        match v {
            0 => ImportStatus::Module,
            1 => ImportStatus::Symbol,
            2 => ImportStatus::Variable,
            3 => ImportStatus::Dynamic,
            4 => ImportStatus::MissingName,
            5 => ImportStatus::MissingModule,
            6 => ImportStatus::BeyondTop,
            7 => ImportStatus::Stdlib,
            8 => ImportStatus::ThirdParty,
            _ => ImportStatus::External,
        }
    }

    /// The import points at code that is not loaded (stdlib, third-party or unknown).
    #[inline]
    pub fn is_external(self) -> bool {
        matches!(self, ImportStatus::Stdlib | ImportStatus::ThirdParty | ImportStatus::External)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            ImportStatus::Module => "module",
            ImportStatus::Symbol => "symbol",
            ImportStatus::Variable => "variable",
            ImportStatus::Dynamic => "dynamic",
            ImportStatus::MissingName => "missing-name",
            ImportStatus::MissingModule => "missing-module",
            ImportStatus::BeyondTop => "beyond-top",
            ImportStatus::Stdlib => "stdlib",
            ImportStatus::ThirdParty => "third-party",
            ImportStatus::External => "external",
        }
    }
}

/// Flags of a resolved import row ([`Imports::flags`]) and of module edges.
pub mod res_flags {
    /// Inside `if TYPE_CHECKING:`: a type-only edge.
    pub const TYPE_ONLY: u8 = 1 << 0;
    /// Inside a conditional block of its scope.
    pub const CONDITIONAL: u8 = 1 << 1;
    /// Guarded by `except ImportError` / `ModuleNotFoundError`.
    pub const TRY: u8 = 1 << 2;
    /// Inside a function or class body (runs when called / at class creation).
    pub const LAZY: u8 = 1 << 3;
    /// `from m import *`.
    pub const STAR: u8 = 1 << 4;
    /// A module-level import its module re-exports: listed in `__all__`, the `import X as X`
    /// idiom, or a public name bound by a package `__init__` without `__all__`.
    pub const EXPORTED: u8 = 1 << 5;
    /// The target was reached through another module's import (a re-export).
    pub const VIA_REEXPORT: u8 = 1 << 6;
    /// The target was reached through a star import of the named module (or of a module it
    /// re-exports from).
    pub const VIA_STAR: u8 = 1 << 7;
    /// Flags an edge keeps only if every import row behind it has them.
    pub const EDGE_ALL: u8 = TYPE_ONLY | CONDITIONAL | TRY | LAZY;
}

/// Phase-1 result of a row: the module part.
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct ModRes {
    /// [`ModId`] of the module part (the deepest prefix for `MissingModule`), else `NONE`.
    pub module: u32,
    /// `DistId` / top-level `Sym` for external rows, else `NONE`.
    pub ext: u32,
    /// `Module` when the module part resolved, else the final status.
    pub status: u8,
}

/// Phase-2 (final) result of a row.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct NameRes {
    pub target: u32,
    /// First re-export hop: the import row of the named module the name was found through.
    pub via: u32,
    pub status: u8,
    pub flags: u8,
}

/// Longest re-export / star chain followed before giving up ([`ImportStatus::Dynamic`]).
const MAX_DEPTH: u32 = 32;
/// Most modules visited through star imports by one lookup.
const MAX_STAR_VISITS: usize = 512;

/// Read-only context of the resolution phases.
pub(super) struct Ctx<'a> {
    pub ix: &'a Index,
    pub mods: &'a ModuleTable,
    pub ns: &'a Namespaces,
    /// `__getattr__` (PEP 562), if interned.
    pub getattr: Option<Sym>,
}

impl Ctx<'_> {
    /// The package a relative import of file `f` at `level` starts from.
    fn relative_base(&self, f: FileId, level: u8) -> Option<ModId> {
        let m = self.mods.file_mod(f)?;
        let mut pkg = if self.ix.kind(self.ix.file_node(f)) == NodeKind::Package { m } else { self.mods.parent(m)? };
        for _ in 1..level {
            pkg = self.mods.parent(pkg)?;
        }
        Some(pkg)
    }

    /// Phase 1: resolves the module part of import `row` of file `f`.
    pub fn module_part(&self, attr: &Attribution, f: FileId, row: usize) -> ModRes {
        let st = self.ix.facts();
        let segs = self.ix.file_view(f).import_module(row);
        let level = st.imports.level[row];
        let start = if level == 0 {
            None
        } else {
            match self.relative_base(f, level) {
                Some(p) => Some(p),
                None => return ModRes { module: NONE, ext: NONE, status: ImportStatus::BeyondTop as u8 },
            }
        };
        let (deep, k) = self.mods.walk(start, segs);
        if k == segs.len()
            && let Some(m) = deep
            && self.mods.node(m).is_some()
        {
            return ModRes { module: m, ext: NONE, status: ImportStatus::Module as u8 };
        }
        // A regular package/module prefix makes the rest a missing submodule; otherwise (no
        // prefix, or only namespace directories) absolute names come from outside.
        let regular = deep.filter(|&d| matches!(self.mods.kind(d), super::ModKind::Module | super::ModKind::Package));
        if level == 0 && regular.is_none() {
            let Some(&top) = segs.first() else {
                // A recovered `from import x`.
                return ModRes { module: NONE, ext: NONE, status: ImportStatus::External as u8 };
            };
            return match attr.classify(self.ix.str(top)) {
                Attributed::Stdlib => ModRes { module: NONE, ext: top.to_raw(), status: ImportStatus::Stdlib as u8 },
                Attributed::ThirdParty(d) => ModRes { module: NONE, ext: d.0, status: ImportStatus::ThirdParty as u8 },
                Attributed::Unknown => ModRes { module: NONE, ext: top.to_raw(), status: ImportStatus::External as u8 },
            };
        }
        ModRes { module: deep.unwrap_or(NONE), ext: NONE, status: ImportStatus::MissingModule as u8 }
    }
}

/// Outcome of a name lookup.
enum Found {
    Hit(NameRes),
    Miss,
    Dynamic,
}

#[inline]
fn hit(status: ImportStatus, target: NodeId) -> Found {
    Found::Hit(NameRes { target, via: NONE, status: status as u8, flags: 0 })
}

#[inline]
fn dynamic() -> NameRes {
    NameRes { target: NONE, via: NONE, status: ImportStatus::Dynamic as u8, flags: 0 }
}

/// Per-thread scratch of phase 2.
#[derive(Default)]
pub(super) struct Scratch {
    visited: Vec<ModId>,
}

/// Phase 2 over phase-1 results of every row.
pub(super) struct Names<'a> {
    pub cx: &'a Ctx<'a>,
    pub p1: &'a [ModRes],
}

impl Names<'_> {
    /// Looks up `name` as an attribute of module `m` (`from m import name`).
    pub fn attribute(&self, m: ModId, name: Sym) -> Option<NameRes> {
        let mut sc = Scratch::default();
        match self.find(m, name, false, NONE, 0, &mut sc) {
            Found::Hit(h) => Some(h),
            Found::Miss => None,
            Found::Dynamic => Some(dynamic()),
        }
    }

    /// Looks up `name` through the star imports of file `f` (its own bindings excluded).
    pub fn through_stars(&self, f: FileId, name: Sym) -> Option<NameRes> {
        let mut sc = Scratch::default();
        match self.stars(&[Some(f), None], name, 0, &mut sc) {
            Found::Hit(h) => Some(h),
            Found::Miss => None,
            Found::Dynamic => Some(dynamic()),
        }
    }

    /// Phase 2: final resolution of import `row` of file `f`.
    pub fn row(&self, f: FileId, row: usize, sc: &mut Scratch) -> NameRes {
        let st = self.cx.ix.facts();
        let fl = st.imports.flags[row];
        let mut flags = 0;
        if fl & import_flags::TYPE_CHECKING != 0 {
            flags |= res_flags::TYPE_ONLY;
        }
        if fl & import_flags::CONDITIONAL != 0 {
            flags |= res_flags::CONDITIONAL;
        }
        if fl & import_flags::TRY_IMPORT_ERROR != 0 {
            flags |= res_flags::TRY;
        }
        if st.imports.scope[row] != 0 {
            flags |= res_flags::LAZY;
        }
        if fl & import_flags::STAR != 0 {
            flags |= res_flags::STAR;
        } else if st.imports.scope[row] == 0 && self.exported(f, row) {
            flags |= res_flags::EXPORTED;
        }
        let p = self.p1[row];
        let mut r = NameRes { target: NONE, via: NONE, status: p.status, flags };
        if p.status != ImportStatus::Module as u8 {
            return r;
        }
        let name = st.imports.name[row].filter(|_| fl & import_flags::FROM != 0);
        let Some(name) = name else {
            // `import a.b [as c]` and `from a import *`: the module itself.
            r.target = self.cx.mods.node(p.module).unwrap_or(NONE);
            return r;
        };
        sc.visited.clear();
        match self.find(p.module, name, false, row as u32, 0, sc) {
            Found::Hit(h) => {
                r.target = h.target;
                r.via = h.via;
                r.status = h.status;
                r.flags |= h.flags;
            }
            Found::Miss => r.status = ImportStatus::MissingName as u8,
            Found::Dynamic => r.status = ImportStatus::Dynamic as u8,
        }
        r
    }

    /// Whether module-level import `row` of file `f` is re-exported.
    fn exported(&self, f: FileId, row: usize) -> bool {
        let ix = self.cx.ix;
        let st = ix.facts();
        let fv = ix.file_view(f);
        let from = st.imports.flags[row] & import_flags::FROM != 0;
        let (alias, name) = (st.imports.alias[row], st.imports.name[row]);
        let bound = match (alias, name) {
            (Some(a), _) => a,
            (None, Some(n)) if from => n,
            _ => match fv.import_module(row).first() {
                Some(&s) => s,
                None => return false,
            },
        };
        // `import x as x` / `from m import x as x`.
        let redundant = alias.is_some_and(|a| {
            if from { Some(a) == name } else { fv.import_module(row) == std::slice::from_ref(&a) }
        });
        if redundant {
            return true;
        }
        if ix.file_facts(f).has(file_flags::ALL_DEFINED) {
            return self.cx.ns.get(f, bound).is_some_and(|e| e.in_all());
        }
        ix.kind(ix.file_node(f)) == NodeKind::Package && ix.str(bound).first() != Some(&b'_')
    }

    /// Looks up attribute `name` of module `m`: a module-level binding of its implementation
    /// (then its stub), else a submodule, else the module's star imports (depth-first, each
    /// module once per lookup). `via_star`: `m` is reached through a star import, so only the
    /// names it exports count (`__all__`, else public names). `skip`: the import row being
    /// resolved or followed, ignored as a binding (`from . import a` in `pkg/__init__` binds
    /// `a` itself, so it must find the submodule, not itself).
    fn find(&self, m: ModId, name: Sym, via_star: bool, skip: u32, depth: u32, sc: &mut Scratch) -> Found {
        let cx = self.cx;
        if depth > MAX_DEPTH {
            return Found::Dynamic;
        }
        let Some(node) = cx.mods.node(m) else { return Found::Miss };
        let files = [cx.ix.file(node), cx.mods.stub(m).and_then(|s| cx.ix.file(s))];
        let mut entry: Option<(FileId, Entry)> = None;
        let (mut in_all, mut has_all) = (false, false);
        for f in files.into_iter().flatten() {
            let ff = cx.ix.file_facts(f);
            has_all |= ff.has(file_flags::ALL_DEFINED) && !ff.has(file_flags::ALL_DYNAMIC);
            if let Some(e) = cx.ns.get(f, name) {
                in_all |= e.in_all();
                if entry.is_none() && e.base() != kind::UNBOUND && !(e.base() == kind::IMPORT && e.r == skip) {
                    entry = Some((f, e));
                }
            }
        }
        if via_star && !(in_all || (!has_all && cx.ix.str(name).first() != Some(&b'_'))) {
            return Found::Miss;
        }
        if let Some((f, e)) = entry {
            return match e.base() {
                kind::DEF => hit(ImportStatus::Symbol, cx.ix.def_node(f, e.r)),
                kind::VAR => hit(ImportStatus::Variable, node),
                _ => self.follow(e.r as usize, depth, sc),
            };
        }
        if (!via_star || in_all)
            && let Some(c) = cx.mods.child(Some(m), name)
            && let Some(v) = cx.mods.node(c)
        {
            return hit(ImportStatus::Module, v);
        }
        match self.stars(&files, name, depth, sc) {
            Found::Miss => {}
            other => return other,
        }
        // PEP 562 module `__getattr__`.
        if let Some(g) = cx.getattr
            && files.into_iter().flatten().any(|f| cx.ns.get(f, g).is_some_and(|e| e.base() == kind::DEF))
        {
            return Found::Dynamic;
        }
        Found::Miss
    }

    /// Looks up `name` through the module-level star imports of `files` (depth-first, each
    /// source module once per lookup).
    fn stars(&self, files: &[Option<FileId>], name: Sym, depth: u32, sc: &mut Scratch) -> Found {
        let mut dynamic = false;
        for &f in files.iter().flatten() {
            for row in self.cx.ns.stars(f) {
                let src = self.p1[row as usize];
                if src.status != ImportStatus::Module as u8 {
                    // Names of unresolved code are unknowable.
                    dynamic = true;
                    continue;
                }
                if sc.visited.contains(&src.module) {
                    continue;
                }
                if sc.visited.len() >= MAX_STAR_VISITS {
                    return Found::Dynamic;
                }
                sc.visited.push(src.module);
                match self.find(src.module, name, true, NONE, depth + 1, sc) {
                    Found::Hit(mut h) => {
                        h.flags |= res_flags::VIA_STAR;
                        return Found::Hit(h);
                    }
                    Found::Dynamic => dynamic = true,
                    Found::Miss => {}
                }
            }
        }
        if dynamic { Found::Dynamic } else { Found::Miss }
    }

    /// Follows module-level import `row` (of the module a name was found in) to what it binds.
    fn follow(&self, row: usize, depth: u32, sc: &mut Scratch) -> Found {
        let cx = self.cx;
        let st = cx.ix.facts();
        let p = self.p1[row];
        let mut found = if p.status != ImportStatus::Module as u8 {
            // Re-exported from code that is not resolved: keep its attribution.
            Found::Hit(NameRes { target: NONE, via: NONE, status: p.status, flags: 0 })
        } else if st.imports.flags[row] & import_flags::FROM != 0 {
            match st.imports.name[row] {
                Some(n) => match self.find(p.module, n, false, row as u32, depth + 1, sc) {
                    Found::Miss => hit(ImportStatus::MissingName, NONE),
                    other => other,
                },
                None => Found::Dynamic,
            }
        } else if st.imports.alias[row].is_some() {
            // `import a.b as c` binds `a.b`.
            hit(ImportStatus::Module, cx.mods.node(p.module).unwrap_or(NONE))
        } else {
            // `import a.b` binds the top-level package `a`.
            let mut top = p.module;
            while let Some(parent) = cx.mods.parent(top) {
                top = parent;
            }
            hit(ImportStatus::Module, cx.mods.node(top).unwrap_or(NONE))
        };
        if let Found::Hit(h) = &mut found {
            h.via = row as u32;
            h.flags |= res_flags::VIA_REEXPORT;
        }
        found
    }
}
