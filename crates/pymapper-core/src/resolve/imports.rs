//! Import resolution: module part (phase 1), then imported names through re-exports and star
//! imports (phase 2). Both phases run in parallel per file against read-only tables.

use super::attrib::{Attributed, Attribution};
use super::modules::{ModId, ModuleTable};
use super::namespace::{Entry, Namespaces, kind};
use crate::extract::events::AssignKind;
use crate::extract::facts::{ValueKind, assign_kind, binding_flags, file_flags, import_flags};
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
    /// Segments of the module part matched by the module table (saturated).
    pub matched: u16,
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
                None => return ModRes { module: NONE, ext: NONE, status: ImportStatus::BeyondTop as u8, matched: 0 },
            }
        };
        let (deep, k) = self.mods.walk(start, segs);
        if k == segs.len()
            && let Some(m) = deep
            && self.mods.node(m).is_some()
        {
            return ModRes { module: m, ext: NONE, status: ImportStatus::Module as u8, matched: sat16(k) };
        }
        // A regular package/module prefix makes the rest a missing submodule; otherwise (no
        // prefix, or only namespace directories) absolute names come from outside.
        let regular = deep.filter(|&d| matches!(self.mods.kind(d), super::ModKind::Module | super::ModKind::Package));
        if level == 0 && regular.is_none() {
            let Some(&top) = segs.first() else {
                // A recovered `from import x`.
                return ModRes { module: NONE, ext: NONE, status: ImportStatus::External as u8, matched: 0 };
            };
            return match attr.classify(self.ix.str(top)) {
                Attributed::Stdlib => ModRes { module: NONE, ext: top.to_raw(), status: ImportStatus::Stdlib as u8, matched: 0 },
                Attributed::ThirdParty(d) => ModRes { module: NONE, ext: d.0, status: ImportStatus::ThirdParty as u8, matched: 0 },
                Attributed::Unknown => ModRes { module: NONE, ext: top.to_raw(), status: ImportStatus::External as u8, matched: 0 },
            };
        }
        ModRes { module: deep.unwrap_or(NONE), ext: NONE, status: ImportStatus::MissingModule as u8, matched: sat16(k) }
    }
}

#[inline]
fn sat16(k: usize) -> u16 {
    u16::try_from(k).unwrap_or(u16::MAX)
}

/// Outcome of a name lookup.
pub(super) enum Found {
    Hit(NameRes),
    Miss,
    Dynamic,
}

#[inline]
fn hit(status: ImportStatus, target: NodeId) -> Found {
    Found::Hit(NameRes { target, via: NONE, status: status as u8, flags: 0 })
}

/// Attributes every module object has (set by the import system).
fn is_module_attr(name: &[u8]) -> bool {
    name.starts_with(b"__")
        && matches!(
            name,
            b"__doc__"
                | b"__name__"
                | b"__file__"
                | b"__path__"
                | b"__spec__"
                | b"__loader__"
                | b"__package__"
                | b"__dict__"
                | b"__builtins__"
                | b"__cached__"
                | b"__annotations__"
        )
}

#[inline]
fn dynamic() -> NameRes {
    NameRes { target: NONE, via: NONE, status: ImportStatus::Dynamic as u8, flags: 0 }
}

/// Per-thread scratch of phase 2.
#[derive(Default)]
pub(super) struct Scratch {
    /// (source module, name) pairs already searched through a star import by the current
    /// lookup. Keyed by name too: a re-export followed inside the lookup searches the same
    /// modules for another name. Bounded by [`MAX_STAR_VISITS`].
    visited: Vec<(ModId, Sym)>,
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
        sc.visited.clear();
        let Some(m) = self.effective_module(Some(f), row, 0, sc) else { return r };
        let name = st.imports.name[row].filter(|_| fl & import_flags::FROM != 0);
        let Some(name) = name else {
            // `import a.b [as c]` and `from a import *`: the module itself.
            r.status = ImportStatus::Module as u8;
            r.target = self.cx.mods.node(m).unwrap_or(NONE);
            return r;
        };
        sc.visited.clear();
        match self.find(m, name, false, row as u32, 0, sc) {
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

    /// The module the module part of `row` names: the phase-1 module, or for a
    /// `MissingModule` row, the module its missing segments name as attributes (`os.path`,
    /// `collections.abc`: modules bound in their parent and registered in `sys.modules`).
    /// `f` is the row's file if known.
    fn effective_module(&self, f: Option<FileId>, row: usize, depth: u32, sc: &mut Scratch) -> Option<ModId> {
        let p = self.p1[row];
        if p.status == ImportStatus::Module as u8 {
            return Some(p.module);
        }
        if p.status != ImportStatus::MissingModule as u8 || p.module == NONE || p.matched == u16::MAX {
            return None;
        }
        let cx = self.cx;
        let f = f.unwrap_or_else(|| self.file_of(row));
        let segs = cx.ix.file_view(f).import_module(row);
        let mut cur = p.module;
        for &seg in segs.get(p.matched as usize..)? {
            match self.find(cur, seg, false, row as u32, depth + 1, sc) {
                Found::Hit(h) if h.status == ImportStatus::Module as u8 && h.target != NONE => {
                    cur = cx.mods.file_mod(cx.ix.file(h.target)?)?;
                }
                _ => return None,
            }
        }
        cx.mods.node(cur).map(|_| cur)
    }

    /// File of store-absolute import row `row` (binary search over the files).
    fn file_of(&self, row: usize) -> FileId {
        let ix = self.cx.ix;
        let (mut lo, mut hi) = (0, ix.file_count());
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            if ix.file_facts(mid).imports.range().end <= row {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        lo
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
                kind::VAR => self.alias(f, e.r as usize, m, depth, sc).unwrap_or_else(|| hit(ImportStatus::Variable, node)),
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
        // PEP 562 module `__getattr__`, or names bound through `globals()` / `exec()`.
        let getattr = |f: FileId| cx.getattr.is_some_and(|g| cx.ns.get(f, g).is_some_and(|e| e.base() == kind::DEF));
        if files.into_iter().flatten().any(|f| cx.ns.is_dynamic(f) || getattr(f)) {
            return Found::Dynamic;
        }
        if !via_star && is_module_attr(cx.ix.str(name)) {
            return hit(ImportStatus::Variable, node);
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
                if sc.visited.contains(&(src.module, name)) {
                    continue;
                }
                if sc.visited.len() >= MAX_STAR_VISITS {
                    return Found::Dynamic;
                }
                sc.visited.push((src.module, name));
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

    /// Follows a module-level alias `X = Y` / `X = a.b` (binding `brow` of file `f`, module
    /// `m`) to the class, function or module the value names; `None` for other values or when
    /// the value does not resolve to one (the name is then a plain variable).
    pub(super) fn alias(&self, f: FileId, brow: usize, m: ModId, depth: u32, sc: &mut Scratch) -> Option<Found> {
        let cx = self.cx;
        let b = &cx.ix.facts().bindings;
        let plain = matches!(assign_kind(b.kind[brow]), AssignKind::Plain | AssignKind::Annotated);
        let single = b.flags[brow] & (binding_flags::UNPACKED | binding_flags::STARRED) == 0;
        if !plain || !single || b.value_kind(brow) != ValueKind::Name {
            return None;
        }
        let chain = cx.ix.file_view(f).binding_value(brow);
        let (&first, rest) = chain.split_first()?;
        let Found::Hit(mut cur) = self.find(m, first, false, NONE, depth + 1, sc) else { return None };
        for &seg in rest {
            if cur.status != ImportStatus::Module as u8 || cur.target == NONE {
                return None;
            }
            let cm = cx.mods.file_mod(cx.ix.file(cur.target)?)?;
            let Found::Hit(mut next) = self.find(cm, seg, false, NONE, depth + 1, sc) else { return None };
            next.flags |= cur.flags;
            if cur.via != NONE {
                next.via = cur.via;
            }
            cur = next;
        }
        let named = cur.status == ImportStatus::Symbol as u8 || cur.status == ImportStatus::Module as u8;
        (named && cur.target != NONE).then_some(Found::Hit(cur))
    }

    /// Follows module-level import `row` (of the module a name was found in) to what it binds.
    fn follow(&self, row: usize, depth: u32, sc: &mut Scratch) -> Found {
        let cx = self.cx;
        let st = cx.ix.facts();
        let p = self.p1[row];
        let from = st.imports.flags[row] & import_flags::FROM != 0;
        let mut found = if !from && st.imports.alias[row].is_none() && p.module != NONE {
            // `import a.b` binds the top-level package `a` (also when `a.b` is missing).
            hit(ImportStatus::Module, cx.mods.node(cx.mods.top(p.module)).unwrap_or(NONE))
        } else {
            match self.effective_module(None, row, depth, sc) {
                // Re-exported from code that is not resolved: keep its attribution.
                None => Found::Hit(NameRes { target: NONE, via: NONE, status: p.status, flags: 0 }),
                Some(m) if from => match st.imports.name[row] {
                    Some(n) => match self.find(m, n, false, row as u32, depth + 1, sc) {
                        Found::Miss => hit(ImportStatus::MissingName, NONE),
                        other => other,
                    },
                    None => Found::Dynamic,
                },
                // `import a.b as c` binds `a.b`.
                Some(m) => hit(ImportStatus::Module, cx.mods.node(m).unwrap_or(NONE)),
            }
        };
        if let Found::Hit(h) = &mut found {
            h.via = row as u32;
            h.flags |= res_flags::VIA_REEXPORT;
        }
        found
    }
}
