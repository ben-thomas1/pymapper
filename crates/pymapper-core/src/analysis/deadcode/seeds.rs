//! Child seeds and same-name sibling rings of the def graph (see the module docs of `deadcode`).

use super::Why;
use super::prop::DefGraph;
use crate::config::EntrypointMatcher;
use crate::extract::facts::{DECL_GETATTR_PREFIX, DefKind, FileView, def_flags, evidence_hash, file_flags, import_flags};
use crate::intern::FxHashSet;
use crate::roots::Origin;
use crate::index::{FileId, Index, NONE, NodeId};
use crate::reach::FileModes;
use crate::intern::Sym;
use crate::resolve::{Imports, class_flags, res_flags};

/// Decorators that wrap a function without registering it anywhere: the decorated def is still
/// only live when called. Matched on the last segment of the decorator's dotted name.
pub const WRAPPERS: &[&[u8]] = &[
    b"wraps",
    b"lru_cache",
    b"cache",
    b"cached_property",
    b"total_ordering",
    b"dataclass",
    b"contextmanager",
    b"asynccontextmanager",
    b"final",
    b"deprecated",
    b"no_type_check",
    b"runtime_checkable",
    b"unique",
    b"singledispatch",
    b"singledispatchmethod",
    b"jit",
    b"njit",
];

/// Names whose presence in a file's evidence means dynamic access to its namespace or its
/// objects' attributes. `getattr` is not among them: the extractor classifies its attribute
/// name ([`file_flags::DYNAMIC_GETATTR`] for a variable one, [`DECL_GETATTR_PREFIX`] rows for a
/// constant prefix, nothing for a constant).
const DYNAMIC_NAMES: &[&[u8]] = &[b"globals", b"locals", b"vars", b"__dict__", b"__getattr__"];

/// `getattr` prefixes at least this long apply to matching defs in every file; shorter ones
/// (`f"_{x}"`) only to the defs of their own file.
const GLOBAL_PREFIX: usize = 3;

/// Per-file facts shared by the def and import passes.
pub(super) struct FileInfo {
    /// The file contributes (origin selected, runtime `.py` file).
    pub included: Vec<bool>,
    /// The file uses dynamic access ([`DYNAMIC_NAMES`], a module-level `__getattr__`, a
    /// variable `getattr`).
    pub dynamic: Vec<bool>,
}

/// Interned names the seed pass compares against.
struct Names<'a> {
    /// Member rules ([`EntrypointMatcher::member`]), when any.
    members: Option<&'a EntrypointMatcher>,
    protocol: Option<Sym>,
    override_: Option<Sym>,
    getattr: Option<Sym>,
    wrappers: Vec<Sym>,
    dynamic: Vec<u32>,
}

impl<'a> Names<'a> {
    fn new(ix: &Index, members: Option<&'a EntrypointMatcher>) -> Names<'a> {
        let g = |s: &[u8]| ix.strings().get(s);
        let mut wrappers: Vec<Sym> = WRAPPERS.iter().filter_map(|w| g(w)).collect();
        wrappers.sort_unstable();
        Names {
            members: members.filter(|m| m.has_members()),
            protocol: g(b"Protocol"),
            override_: g(b"override"),
            getattr: g(b"__getattr__"),
            wrappers,
            dynamic: DYNAMIC_NAMES.iter().map(|n| evidence_hash(n)).collect(),
        }
    }
}

/// Raises `seed` to `why` when that is a stronger seed.
#[inline]
fn put(seed: &mut u8, why: Why) {
    if why.seed_strength() > Why::from_u8(*seed).seed_strength() {
        *seed = why as u8;
    }
}

/// Reused per-file buffers.
#[derive(Default)]
struct Scratch {
    /// Per def row of the file: bit 0 Protocol class, bit 1 incomplete class, bit 2 public.
    marks: Vec<u8>,
    /// `(parent row, name, node)` of the file's defs, for sibling rings.
    named: Vec<(u32, Sym, NodeId)>,
    /// Dotted decorator names being matched.
    buf: Vec<u8>,
}

const PROTOCOL: u8 = 1;
const INCOMPLETE: u8 = 2;
const PUBLIC: u8 = 4;
/// A class with a loaded ancestor defined in a module that uses dynamic access (name-based
/// dispatch such as `getattr(self, "_print_" + name)` in a base printer or visitor).
const DYNAMIC_BASE: u8 = 8;

/// Constant `getattr` name prefixes ([`DECL_GETATTR_PREFIX`]) of the included files.
struct Prefixes<'a> {
    /// Prefixes of [`GLOBAL_PREFIX`] bytes or more, from any included file.
    global: FxHashSet<&'a [u8]>,
    /// Their distinct lengths, ascending.
    lens: Vec<usize>,
    /// Shorter prefixes of the current file.
    local: Vec<&'a [u8]>,
}

impl<'a> Prefixes<'a> {
    fn new(ix: &'a Index, included: &[bool]) -> Prefixes<'a> {
        let mut p = Prefixes { global: FxHashSet::default(), lens: Vec::new(), local: Vec::new() };
        let decls = &ix.facts().decls;
        for f in 0..ix.file_count() {
            if !included[f as usize] {
                continue;
            }
            for r in ix.file_facts(f).decls.range() {
                let pre = ix.str(decls.name[r]);
                if decls.kind[r] == DECL_GETATTR_PREFIX && pre.len() >= GLOBAL_PREFIX && p.global.insert(pre) {
                    p.lens.push(pre.len());
                }
            }
        }
        p.lens.sort_unstable();
        p.lens.dedup();
        p
    }

    /// Loads the short prefixes of file `f`.
    fn load_local(&mut self, ix: &'a Index, f: FileId) {
        self.local.clear();
        let decls = &ix.facts().decls;
        for r in ix.file_facts(f).decls.range() {
            let pre = ix.str(decls.name[r]);
            if decls.kind[r] == DECL_GETATTR_PREFIX && pre.len() < GLOBAL_PREFIX {
                self.local.push(pre);
            }
        }
    }

    /// Whether `name` starts with a global prefix or a local one of the loaded file.
    #[inline]
    fn matches(&self, name: &[u8]) -> bool {
        self.local.iter().any(|p| name.starts_with(p))
            || self.lens.iter().take_while(|&&l| l <= name.len()).any(|&l| self.global.contains(&name[..l]))
    }
}

/// Fills the child seeds and sibling rings of `g` for the files `include` selects, and returns
/// the per-file facts. Files in library mode (`modes`) get library-mode public seeds, the
/// others application-mode exported seeds. O(defs + decorators of decorated defs + bases of
/// classes + import rows).
pub(super) fn fill(
    ix: &Index,
    imp: &Imports,
    g: &mut DefGraph<'_>,
    modes: &FileModes,
    members: Option<&EntrypointMatcher>,
    include: impl Fn(FileId) -> bool,
) -> FileInfo {
    let names = Names::new(ix, members);
    let files = ix.file_count();
    let mut info = FileInfo { included: Vec::with_capacity(files as usize), dynamic: Vec::with_capacity(files as usize) };
    let mut sc = Scratch::default();
    for f in 0..files {
        let inc = include(f);
        info.included.push(inc);
        info.dynamic.push(inc && is_dynamic(ix, f, &names));
    }
    let mut prefixes = Prefixes::new(ix, &info.included);
    for f in 0..files {
        if info.included[f as usize] {
            prefixes.load_local(ix, f);
            file_seeds(ix, imp, g, f, modes.library(f), &info.dynamic, &names, &prefixes, &mut sc);
        }
    }
    external_overrides(ix, g, &info.included);
    if modes.library_units() > 0 {
        star_exports(ix, imp, g, &info.included, modes);
    }
    if modes.library_units() < modes.units() {
        // Application mode: re-export targets, wherever they are defined.
        let st = ix.facts();
        for f in 0..files {
            if !info.included[f as usize] || modes.library(f) {
                continue;
            }
            for row in ix.file_facts(f).imports.range() {
                if imp.flags(row) & res_flags::EXPORTED != 0
                    && st.imports.scope[row] == 0
                    && let Some(t) = imp.target(row)
                    && ix.kind(t).is_def()
                {
                    put(&mut g.seed[t as usize], Why::Exported);
                }
            }
        }
    }
    info
}

/// No package/module on the path of module node `m` is private (dunders such as `__main__`
/// excepted), as for the library-mode roots.
fn module_is_public(ix: &Index, m: NodeId) -> bool {
    (1..=ix.depth(m)).all(|k| {
        let name = ix.name(ix.ancestor_at(m, k));
        !name.starts_with(b"_") || (name.len() > 4 && name.ends_with(b"__"))
    })
}

/// Library mode: the module-level defs that a public module of a library-mode file re-exports
/// through a module-level `from m import *` of a loaded module (its `__all__` when static, else
/// its public names).
fn star_exports(ix: &Index, imp: &Imports, g: &mut DefGraph<'_>, included: &[bool], modes: &FileModes) {
    let st = ix.facts();
    for f in 0..ix.file_count() {
        if !included[f as usize] || !modes.library(f) || !module_is_public(ix, ix.file_node(f)) {
            continue;
        }
        for row in ix.file_facts(f).imports.range() {
            if st.imports.flags[row] & import_flags::STAR == 0 || st.imports.scope[row] != 0 {
                continue;
            }
            let m = imp.module_node(row);
            let Some(src) = (m != NONE).then(|| ix.file(m)).flatten() else { continue };
            if !included[src as usize] || ix.file_node(src) != m {
                continue;
            }
            let ff = ix.file_facts(src);
            let restrict = ff.has(file_flags::ALL_DEFINED) && !ff.has(file_flags::ALL_DYNAMIC);
            let base = ff.defs.start as usize;
            for d in 1..ff.defs.len as usize {
                let r = base + d;
                if st.defs.parent[r] != 0 {
                    continue;
                }
                let exported = match st.defs.name[r] {
                    Some(s) if restrict => imp.in_all(src, s),
                    Some(_) => st.defs.flags[r] & (def_flags::PRIVATE | def_flags::DUNDER) == 0,
                    None => false,
                };
                if exported {
                    put(&mut g.seed[m as usize + d], Why::StarExport);
                }
            }
        }
    }
}

/// Methods of first-party classes that override a method of a third-party or stdlib class
/// later in the MRO of some first-party class (directly, through a subclass, or as a mixin
/// before the external base: `class C(Mixin, ExtBase)`): the framework calls them, so they
/// are live with their class ([`Why::ExternalOverride`]). When the base is not loaded, the
/// methods of base-less first-party mixins of the class get [`Why::ExternalBase`]. O(classes with an external ancestor
/// x (MRO + methods of the classes in it) x log).
fn external_overrides(ix: &Index, g: &mut DefGraph<'_>, included: &[bool]) {
    let cl = g.classes;
    let external = |k: u32| ix.file(cl.node(k)).is_some_and(|f| ix.file_origin(f) != Origin::FirstParty);
    let mut names: Vec<Sym> = Vec::new();
    for c in 0..cl.len() {
        // Base not loaded: a mixin (a first-party class without bases of its own) among the
        // class's direct bases may override the unknown base's methods, like the class itself
        // ([`Why::ExternalBase`]).
        if cl.flags(c) & class_flags::INCOMPLETE != 0 && ix.file(cl.node(c)).is_some_and(|f| included[f as usize]) {
            for &k in cl.bases(c) {
                let kv = cl.node(k);
                if cl.bases(k).is_empty() && cl.flags(k) & class_flags::INCOMPLETE == 0 && ix.file(kv).is_some_and(|f| included[f as usize]) {
                    for m in ix.children(kv) {
                        if ix.kind(m) == crate::index::NodeKind::Function {
                            put(&mut g.seed[m as usize], Why::ExternalBase);
                        }
                    }
                }
            }
        }
        let mro = cl.mro(c);
        if !ix.file(cl.node(c)).is_some_and(|f| included[f as usize]) || !mro.iter().skip(1).any(|&k| external(k)) {
            continue;
        }
        names.clear();
        // From the most basic class up: the external names seen so far are overridden by the
        // first-party classes before them.
        for &k in mro.iter().rev() {
            let kv = cl.node(k);
            if external(k) {
                let at = names.len();
                for ch in ix.children(kv) {
                    if ix.kind(ch) == crate::index::NodeKind::Function {
                        names.push(ix.name_sym(ch));
                    }
                }
                if names.len() > at {
                    names.sort_unstable();
                    names.dedup();
                }
            } else if !names.is_empty() && ix.file(kv).is_some_and(|f| included[f as usize]) {
                for m in ix.children(kv) {
                    if ix.kind(m) == crate::index::NodeKind::Function && names.binary_search(&ix.name_sym(m)).is_ok() {
                        put(&mut g.seed[m as usize], Why::ExternalOverride);
                    }
                }
            }
        }
    }
}

/// Whether file `f` uses dynamic access: a variable `getattr`, a [`DYNAMIC_NAMES`] name or a
/// module-level `__getattr__`.
fn is_dynamic(ix: &Index, f: FileId, names: &Names<'_>) -> bool {
    if ix.file_facts(f).has(file_flags::DYNAMIC_GETATTR) {
        return true;
    }
    let view = ix.file_view(f);
    let ev = view.evidence();
    if names.dynamic.iter().any(|h| ev.binary_search(h).is_ok()) {
        return true;
    }
    let Some(ga) = names.getattr else { return false };
    let defs = &ix.facts().defs;
    ix.file_facts(f).defs.range().skip(1).any(|r| defs.parent[r] == 0 && defs.name[r] == Some(ga))
}

#[allow(clippy::too_many_arguments)]
fn file_seeds(
    ix: &Index,
    imp: &Imports,
    g: &mut DefGraph<'_>,
    f: FileId,
    library: bool,
    dynamic_files: &[bool],
    names: &Names<'_>,
    prefixes: &Prefixes<'_>,
    sc: &mut Scratch,
) {
    let dynamic = dynamic_files[f as usize];
    let m = ix.file_node(f);
    if m == NONE {
        return;
    }
    let ff = ix.file_facts(f);
    let view = ix.file_view(f);
    let defs = &ix.facts().defs;
    let base = ff.defs.start as usize;
    let n = ff.defs.len as usize;
    sc.marks.clear();
    sc.marks.resize(n, 0);
    sc.marks[0] = PUBLIC;
    let mut has_dups = false;
    let exports_static = ff.has(file_flags::ALL_DEFINED) && !ff.has(file_flags::ALL_DYNAMIC);
    for d in 1..n {
        let row = base + d;
        let v = m + d as u32;
        has_dups |= ix.dup(v) > 0;
        let kind = DefKind::from_u8(defs.kind[row]);
        let flags = defs.flags[row];
        let parent = defs.parent[row] as usize;
        let pmark = sc.marks[parent];
        let in_class = parent != 0 && DefKind::from_u8(defs.kind[base + parent]) == DefKind::Class;
        let mut mark = 0;
        if kind == DefKind::Class {
            for r in view.bases(d as u32) {
                if ix.facts().bases.keyword[r].is_none() && view.base_chain(r).last().copied() == names.protocol && names.protocol.is_some() {
                    mark |= PROTOCOL;
                }
            }
            if let Some(c) = g.classes.id(ix, v) {
                let cl = g.classes;
                if cl.flags(c) & class_flags::INCOMPLETE != 0 {
                    mark |= INCOMPLETE;
                }
                let dyn_ancestor = cl.mro(c).iter().skip(1).any(|&a| ix.file(cl.node(a)).is_some_and(|af| dynamic_files[af as usize]));
                if dyn_ancestor {
                    mark |= DYNAMIC_BASE;
                }
            }
        }
        if flags & def_flags::PRIVATE == 0 && (parent == 0 || (in_class && pmark & PUBLIC != 0)) {
            mark |= PUBLIC;
        }
        sc.marks[d] = mark;
        let seed = &mut g.seed[v as usize];
        if in_class {
            if kind == DefKind::Class
                || flags & (def_flags::DUNDER | def_flags::ABSTRACT) != 0
                || pmark & PROTOCOL != 0
            {
                put(seed, Why::Member);
            } else if pmark & INCOMPLETE != 0 {
                put(seed, Why::ExternalBase);
            }
            if let Some(mt) = names.members
                && kind == DefKind::Function
                && is_member(ix, &view, mt, d as u32, defs.name[row], defs.decorators_len[row], &mut sc.buf)
            {
                put(seed, Why::Member);
            }
        } else if flags & def_flags::DUNDER != 0 {
            put(seed, Why::Member);
        }
        if flags & def_flags::OTHER_DECORATOR != 0 {
            for r in view.decorators(d as u32) {
                let chain = view.decorator_chain(r);
                match chain.last() {
                    Some(&s) if Some(s) == names.override_ => put(seed, Why::Member),
                    Some(s) if names.wrappers.binary_search(s).is_ok() => {}
                    _ => put(seed, Why::Decorated),
                }
            }
        }
        if dynamic {
            put(seed, Why::Dynamic);
        } else if in_class && pmark & DYNAMIC_BASE != 0 {
            put(seed, Why::DynamicBase);
        } else if let Some(s) = defs.name[row]
            && prefixes.matches(ix.str(s))
        {
            put(seed, Why::DynamicPrefix);
        }
        if library {
            if mark & PUBLIC != 0 {
                put(seed, Why::LibraryPublic);
            }
            if in_class && kind == DefKind::Function && flags & (def_flags::PRIVATE | def_flags::DUNDER) == 0 {
                put(seed, Why::ApiMethod);
            }
        } else if parent == 0
            && exports_static
            && let Some(s) = defs.name[row]
            && imp.in_all(f, s)
        {
            put(seed, Why::Exported);
        }
    }
    if has_dups {
        siblings(ix, g, f, sc);
    }
}

/// Whether method `d` of a file matches a member rule by name or by a decorator's dotted name.
fn is_member(ix: &Index, view: &FileView<'_>, mt: &EntrypointMatcher, d: u32, name: Option<Sym>, decorators: u8, buf: &mut Vec<u8>) -> bool {
    if name.is_some_and(|s| mt.member(ix.str(s))) {
        return true;
    }
    if decorators == 0 {
        return false;
    }
    for r in view.decorators(d) {
        buf.clear();
        for (i, &s) in view.decorator_chain(r).iter().enumerate() {
            if i > 0 {
                buf.push(b'.');
            }
            buf.extend_from_slice(ix.str(s));
        }
        if !buf.is_empty() && mt.member(buf) {
            return true;
        }
    }
    false
}

/// Links same-name siblings of file `f` into rings.
fn siblings(ix: &Index, g: &mut DefGraph<'_>, f: FileId, sc: &mut Scratch) {
    let ff = ix.file_facts(f);
    let defs = &ix.facts().defs;
    let m = ix.file_node(f);
    sc.named.clear();
    for d in 1..ff.defs.len as usize {
        let row = ff.defs.start as usize + d;
        if let Some(s) = defs.name[row] {
            sc.named.push((defs.parent[row], s, m + d as u32));
        }
    }
    sc.named.sort_unstable();
    let mut i = 0;
    while i < sc.named.len() {
        let mut j = i + 1;
        while j < sc.named.len() && sc.named[j].0 == sc.named[i].0 && sc.named[j].1 == sc.named[i].1 {
            j += 1;
        }
        if j - i > 1 {
            for k in i..j {
                let next = if k + 1 < j { sc.named[k + 1].2 } else { sc.named[i].2 };
                g.siblings.insert(sc.named[k].2, next);
            }
        }
        i = j;
    }
}
