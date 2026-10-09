//! Child seeds and same-name sibling rings of the def graph (see the module docs of `deadcode`).

use super::Why;
use super::prop::DefGraph;
use crate::extract::facts::{DefKind, def_flags, evidence_hash, file_flags, import_flags};
use crate::index::{FileId, Index, NONE, NodeId};
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
/// objects' attributes.
const DYNAMIC_NAMES: &[&[u8]] = &[b"getattr", b"globals", b"locals", b"vars", b"__dict__", b"__getattr__"];

/// Per-file facts shared by the def and import passes.
pub(super) struct FileInfo {
    /// The file contributes (origin selected, runtime `.py` file).
    pub included: Vec<bool>,
    /// The file uses dynamic access ([`DYNAMIC_NAMES`] or a module-level `__getattr__`).
    pub dynamic: Vec<bool>,
}

/// Interned names the seed pass compares against.
struct Names {
    protocol: Option<Sym>,
    override_: Option<Sym>,
    getattr: Option<Sym>,
    wrappers: Vec<Sym>,
    dynamic: Vec<u32>,
}

impl Names {
    fn new(ix: &Index) -> Names {
        let g = |s: &[u8]| ix.strings().get(s);
        let mut wrappers: Vec<Sym> = WRAPPERS.iter().filter_map(|w| g(w)).collect();
        wrappers.sort_unstable();
        Names {
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
}

const PROTOCOL: u8 = 1;
const INCOMPLETE: u8 = 2;
const PUBLIC: u8 = 4;

/// Fills the child seeds and sibling rings of `g` for the files `include` selects, and returns
/// the per-file facts. `library`: library-mode public seeds; otherwise application-mode
/// exported seeds. O(defs + decorators of decorated defs + bases of classes + import rows).
pub(super) fn fill(ix: &Index, imp: &Imports, g: &mut DefGraph<'_>, library: bool, include: impl Fn(FileId) -> bool) -> FileInfo {
    let names = Names::new(ix);
    let files = ix.file_count();
    let mut info = FileInfo { included: Vec::with_capacity(files as usize), dynamic: Vec::with_capacity(files as usize) };
    let mut sc = Scratch::default();
    for f in 0..files {
        let inc = include(f);
        info.included.push(inc);
        let dynamic = inc && is_dynamic(ix, f, &names);
        info.dynamic.push(dynamic);
        if inc {
            file_seeds(ix, imp, g, f, library, dynamic, &names, &mut sc);
        }
    }
    if library {
        star_exports(ix, imp, g, &info.included);
    } else {
        // Re-export targets, wherever they are defined.
        let st = ix.facts();
        for f in 0..files {
            if !info.included[f as usize] {
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

/// Library mode: the module-level defs that a public module re-exports through a module-level
/// `from m import *` of a loaded module (its `__all__` when static, else its public names).
fn star_exports(ix: &Index, imp: &Imports, g: &mut DefGraph<'_>, included: &[bool]) {
    let st = ix.facts();
    for f in 0..ix.file_count() {
        if !included[f as usize] || !module_is_public(ix, ix.file_node(f)) {
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

/// Whether file `f` uses dynamic access.
fn is_dynamic(ix: &Index, f: FileId, names: &Names) -> bool {
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
    dynamic: bool,
    names: &Names,
    sc: &mut Scratch,
) {
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
            if let Some(c) = g.classes.id(ix, v)
                && g.classes.flags(c) & class_flags::INCOMPLETE != 0
            {
                mark |= INCOMPLETE;
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
