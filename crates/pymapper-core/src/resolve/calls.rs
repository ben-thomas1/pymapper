//! Call resolution: every call site, uncalled decorator and method override of the index to
//! call edges with a [`Confidence`] and a [`Reason`]. See the `resolve` module docs.

use rayon::prelude::*;

use super::Imports;
use super::builtins::{BUILTIN_METHODS, BUILTINS, SymSet};
use super::classes::{ClassId, Classes, MethodIndex, class_flags, mkind};
use super::lookup::{Cx, Locals, R, Val};
use crate::extract::events::Receiver;
use crate::extract::facts::{base_flags, call_flags, dec_flags};
use crate::index::{FileId, Index, NONE, NodeId, NodeKind};
use crate::intern::Sym;
use crate::timing;
use std::time::Instant;

/// How sure the resolver is of an edge (ordered: `Certain` is best).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[repr(u8)]
pub enum Confidence {
    /// Statically determined (lexical scope, import, MRO of `self`/`cls`/`super()`).
    Certain = 0,
    /// Depends on a hint (`x = Foo()`, `self.x = Foo()`), an alias, a rebinding in the same
    /// scope, an override, or an unresolved base earlier in the MRO.
    Likely = 1,
    /// A guess: the only method of that name, or a deeper chain of hints.
    Possible = 2,
    /// No target.
    Unresolved = 3,
}

impl Confidence {
    #[inline]
    pub fn from_u8(v: u8) -> Confidence {
        match v {
            0 => Confidence::Certain,
            1 => Confidence::Likely,
            2 => Confidence::Possible,
            _ => Confidence::Unresolved,
        }
    }

    /// One level less sure (`Possible` stays `Possible`; `Unresolved` stays).
    #[inline]
    pub fn down(self) -> Confidence {
        match self {
            Confidence::Certain => Confidence::Likely,
            Confidence::Likely | Confidence::Possible => Confidence::Possible,
            Confidence::Unresolved => Confidence::Unresolved,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Confidence::Certain => "certain",
            Confidence::Likely => "likely",
            Confidence::Possible => "possible",
            Confidence::Unresolved => "unresolved",
        }
    }
}

/// Why a call resolved the way it did (one per call site and per edge). The first group has
/// targets (edges), the second group describes unresolved calls.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[repr(u8)]
pub enum Reason {
    /// A def of an enclosing function (or class body) scope.
    Local = 0,
    /// A module-level def of the caller's file.
    Global = 1,
    /// A name bound by an import (also through re-exports and star imports).
    Imported = 2,
    /// An attribute of a module (`mod.f()`, `pkg.mod.f()`).
    ModuleAttr = 3,
    /// Through an alias assignment (`f = g`, `X = mod.Y`).
    Alias = 4,
    /// `self.m()`: the MRO of the enclosing class.
    SelfMethod = 5,
    /// `cls.m()`: the MRO of the enclosing class.
    ClsMethod = 6,
    /// `super().m()`: the MRO of the enclosing class after it.
    SuperMethod = 7,
    /// An attribute of a class (`Cls.m()`, `Outer.Inner()`), through its MRO.
    ClassAttr = 8,
    /// Calls a class: the edge to the class itself.
    Constructor = 9,
    /// The `__init__` / `__new__` an instantiation runs (edge next to the `Constructor` one).
    Init = 10,
    /// Calls an instance: its `__call__`.
    CallDunder = 11,
    /// A method of an instance built in the caller's scopes (`x = Foo(); x.m()`).
    LocalHint = 12,
    /// A method of an instance bound at module level (`app = App()` in any module).
    GlobalHint = 13,
    /// A method of an instance attribute (`self.x = Foo()` in a method, then `self.x.m()`).
    AttrHint = 14,
    /// `self.m()` where only subclasses of the enclosing class define `m`.
    SubclassMethod = 15,
    /// Receiver unknown; the only (non-dunder) method of that name in the loaded code.
    UniqueMethod = 16,
    /// An uncalled decorator (`@deco`), called with the decorated function.
    Decorator = 17,
    /// Liveness edge: a base method to the method overriding it (dynamic dispatch).
    Override = 18,
    /// A builtin (`len`, `print`, `dict`, ...): no target.
    Builtin = 19,
    /// Code that is not loaded (stdlib, third-party, unknown origin).
    External = 20,
    /// An attribute of a class with a base that is not loaded.
    ExternalBase = 21,
    /// Names bound dynamically: star import of unresolved code, `__getattr__`, `globals()`.
    Dynamic = 22,
    /// The callee is a parameter.
    Parameter = 23,
    /// The callee is a variable without a usable hint.
    Variable = 24,
    /// Call on an expression result (`f()()`, `a[0].m()`, `"".join()`).
    UnknownReceiver = 25,
    /// The name is bound nowhere (not even as a builtin).
    NotFound = 26,
    /// A loaded module or class has no such attribute.
    MissingAttr = 27,
    /// The callee is a module.
    NotCallable = 28,
    /// Receiver unknown; several methods have that name.
    Ambiguous = 29,
    /// Receiver unknown; the name is a method of builtin types (`append`, `join`, ...).
    BuiltinMethod = 30,
    /// An attribute of a function object.
    FunctionAttr = 31,
    /// An import that did not resolve (missing module or name, above the top package).
    UnresolvedImport = 32,
    /// `self` / `cls` / `super()` outside a method.
    NoClass = 33,
}

impl Reason {
    pub const COUNT: usize = 34;

    #[inline]
    pub fn from_u8(v: u8) -> Reason {
        const ALL: [Reason; Reason::COUNT] = [
            Reason::Local,
            Reason::Global,
            Reason::Imported,
            Reason::ModuleAttr,
            Reason::Alias,
            Reason::SelfMethod,
            Reason::ClsMethod,
            Reason::SuperMethod,
            Reason::ClassAttr,
            Reason::Constructor,
            Reason::Init,
            Reason::CallDunder,
            Reason::LocalHint,
            Reason::GlobalHint,
            Reason::AttrHint,
            Reason::SubclassMethod,
            Reason::UniqueMethod,
            Reason::Decorator,
            Reason::Override,
            Reason::Builtin,
            Reason::External,
            Reason::ExternalBase,
            Reason::Dynamic,
            Reason::Parameter,
            Reason::Variable,
            Reason::UnknownReceiver,
            Reason::NotFound,
            Reason::MissingAttr,
            Reason::NotCallable,
            Reason::Ambiguous,
            Reason::BuiltinMethod,
            Reason::FunctionAttr,
            Reason::UnresolvedImport,
            Reason::NoClass,
        ];
        ALL[(v as usize).min(Reason::COUNT - 1)]
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Reason::Local => "local",
            Reason::Global => "global",
            Reason::Imported => "imported",
            Reason::ModuleAttr => "module-attr",
            Reason::Alias => "alias",
            Reason::SelfMethod => "self-method",
            Reason::ClsMethod => "cls-method",
            Reason::SuperMethod => "super-method",
            Reason::ClassAttr => "class-attr",
            Reason::Constructor => "constructor",
            Reason::Init => "init",
            Reason::CallDunder => "call-dunder",
            Reason::LocalHint => "local-hint",
            Reason::GlobalHint => "global-hint",
            Reason::AttrHint => "attr-hint",
            Reason::SubclassMethod => "subclass-method",
            Reason::UniqueMethod => "unique-method",
            Reason::Decorator => "decorator",
            Reason::Override => "override",
            Reason::Builtin => "builtin",
            Reason::External => "external",
            Reason::ExternalBase => "external-base",
            Reason::Dynamic => "dynamic",
            Reason::Parameter => "parameter",
            Reason::Variable => "variable",
            Reason::UnknownReceiver => "unknown-receiver",
            Reason::NotFound => "not-found",
            Reason::MissingAttr => "missing-attr",
            Reason::NotCallable => "not-callable",
            Reason::Ambiguous => "ambiguous",
            Reason::BuiltinMethod => "builtin-method",
            Reason::FunctionAttr => "function-attr",
            Reason::UnresolvedImport => "unresolved-import",
            Reason::NoClass => "no-class",
        }
    }
}

/// One call edge (16 bytes).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CallEdge {
    /// The calling node: the enclosing def, or the file's package/module node (`<module>`).
    pub src: NodeId,
    /// The called class/function node.
    pub dst: NodeId,
    /// Store-absolute row of the site: a call row (`Index::facts().calls`), a decorator row
    /// for [`Reason::Decorator`], `NONE` for [`Reason::Override`].
    pub site: u32,
    pub conf: Confidence,
    pub reason: Reason,
}

/// Packs a call row's outcome into one byte (confidence in the top two bits).
#[inline]
fn pack(conf: Confidence, reason: Reason) -> u8 {
    ((conf as u8) << 6) | reason as u8
}

/// Counters and phase times of [`resolve_calls`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CallStats {
    pub calls: u64,
    /// Call rows per [`Confidence`].
    pub by_conf: [u64; 4],
    /// Call rows per [`Reason`].
    pub by_reason: [u64; Reason::COUNT],
    pub edges: u64,
    /// Edges per [`Reason`].
    pub edges_by_reason: [u64; Reason::COUNT],
    pub classes: u64,
    /// Classes with a base that is not loaded.
    pub incomplete_classes: u64,
    /// Classes without a C3 linearisation.
    pub no_c3: u64,
    pub classes_time: std::time::Duration,
    pub mro_time: std::time::Duration,
    pub calls_time: std::time::Duration,
}

impl Default for CallStats {
    fn default() -> Self {
        CallStats {
            calls: 0,
            by_conf: [0; 4],
            by_reason: [0; Reason::COUNT],
            edges: 0,
            edges_by_reason: [0; Reason::COUNT],
            classes: 0,
            incomplete_classes: 0,
            no_c3: 0,
            classes_time: std::time::Duration::ZERO,
            mro_time: std::time::Duration::ZERO,
            calls_time: std::time::Duration::ZERO,
        }
    }
}

/// Runs `per(state, file, out)` over chunks of consecutive files in parallel and returns the
/// chunk outputs in file order (state `S` is per chunk, reused across its files).
pub(super) fn par_chunks<S: Default, O: Default + Send>(files: usize, per: impl Fn(&mut S, FileId, &mut O) + Sync) -> Vec<O> {
    let chunk = super::namespace::chunk_size(files);
    (0..files.div_ceil(chunk))
        .into_par_iter()
        .map(|c| {
            let mut state = S::default();
            let mut out = O::default();
            for f in c * chunk..files.min((c + 1) * chunk) {
                per(&mut state, f as FileId, &mut out);
            }
            out
        })
        .collect()
}

/// Resolved calls of an index (see the module docs of `resolve`).
pub struct ResolvedCalls {
    classes: Classes,
    methods: MethodIndex,
    /// Per call row (store-absolute): [`pack`]ed confidence and reason.
    info: Vec<u8>,
    edges: Vec<CallEdge>,
    /// First edge of every file (`files + 1` entries).
    edge_start: Vec<u32>,
    stats: CallStats,
}

impl ResolvedCalls {
    /// Every edge, files in FileId order; per file: call edges in call row order, then
    /// decorator edges, then override edges.
    #[inline]
    pub fn edges(&self) -> &[CallEdge] {
        &self.edges
    }

    /// Edges whose site is in file `f` (override edges: the overriding class's file).
    #[inline]
    pub fn file_edges(&self, f: FileId) -> &[CallEdge] {
        &self.edges[self.edge_start[f as usize] as usize..self.edge_start[f as usize + 1] as usize]
    }

    /// Outcome of call row `row` (store-absolute): the confidence of its best edge (or
    /// `Unresolved`) and the reason.
    #[inline]
    pub fn call(&self, row: usize) -> (Confidence, Reason) {
        let v = self.info[row];
        (Confidence::from_u8(v >> 6), Reason::from_u8(v & 0x3f))
    }

    /// Number of call rows (= `Index::facts().calls.len()`).
    #[inline]
    pub fn len(&self) -> usize {
        self.info.len()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.info.is_empty()
    }

    /// The class table (members and MROs).
    #[inline]
    pub fn classes(&self) -> &Classes {
        &self.classes
    }

    /// The MRO of class node `v` as class nodes (itself first); `None` if `v` is not a class.
    pub fn mro_nodes<'a>(&'a self, ix: &Index, v: NodeId) -> Option<impl Iterator<Item = NodeId> + 'a> {
        let c = self.classes.id(ix, v)?;
        Some(self.classes.mro(c).iter().map(|&k| self.classes.node(k)))
    }

    #[inline]
    pub fn stats(&self) -> &CallStats {
        &self.stats
    }

    /// Heap bytes (capacity) of every table.
    pub fn heap_bytes(&self) -> usize {
        self.classes.heap_bytes()
            + self.methods.heap_bytes()
            + self.info.capacity()
            + self.edges.capacity() * size_of::<CallEdge>()
            + self.edge_start.capacity() * 4
    }
}

/// Interned names the resolver compares against.
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct Names {
    pub self_: Option<Sym>,
    pub init: Option<Sym>,
    pub new: Option<Sym>,
    pub call: Option<Sym>,
    pub getattr: Option<Sym>,
    pub class_: Option<Sym>,
    pub metaclass: Option<Sym>,
    pub object: Option<Sym>,
    /// Base names that add nothing callable (`Generic[T]`, `Protocol`, `ABC`).
    pub markers: [Option<Sym>; 3],
}

impl Names {
    fn new(ix: &Index) -> Names {
        let g = |s: &[u8]| ix.strings().get(s);
        Names {
            self_: g(b"self"),
            init: g(b"__init__"),
            new: g(b"__new__"),
            call: g(b"__call__"),
            getattr: g(b"__getattr__"),
            class_: g(b"__class__"),
            metaclass: g(b"metaclass"),
            object: g(b"object"),
            markers: [g(b"Generic"), g(b"Protocol"), g(b"ABC")],
        }
    }
}

/// Builtin name tables.
pub(super) struct Tables {
    pub builtins: SymSet,
    pub builtin_methods: SymSet,
}

impl Tables {
    fn new(ix: &Index) -> Tables {
        Tables { builtins: SymSet::new(ix.strings(), BUILTINS), builtin_methods: SymSet::new(ix.strings(), BUILTIN_METHODS) }
    }
}

const _: () = assert!(size_of::<CallEdge>() == 16);

/// Most subclass methods considered for one `self.m()` that the class itself lacks.
const MAX_SUBCLASS_CANDIDATES: usize = 64;

/// Resolves every call site of `ix` against the resolved imports (see the `resolve` module
/// docs). Runs on the current rayon pool. Phases are recorded with [`timing`] as
/// `resolve.classes`, `resolve.mro` and `resolve.calls`.
pub fn resolve_calls(ix: &Index, imp: &Imports) -> ResolvedCalls {
    let names = Names::new(ix);
    let tables = Tables::new(ix);
    let t0 = Instant::now();
    let (mut classes, methods) = timing::time("resolve.classes", || {
        rayon::join(|| Classes::build(ix, names.self_, names.getattr), || MethodIndex::build(ix))
    });
    let t1 = Instant::now();
    timing::time("resolve.mro", || {
        let (bases, base_start, flags) = {
            let cx = Cx { ix, imp, classes: &classes, tables: &tables, names };
            class_bases(&cx)
        };
        classes.set_bases(bases, base_start, &flags);
        classes.linearize();
    });
    let t2 = Instant::now();
    let files = ix.file_count() as usize;
    let chunks = timing::time("resolve.calls", || {
        let cx = Cx { ix, imp, classes: &classes, tables: &tables, names };
        par_chunks(files, |sc: &mut FileScratch, f, out: &mut CallChunk| file_calls(&cx, &methods, sc, f, out))
    });
    let (info, edges, edge_start) = merge_chunks(chunks, files);
    debug_assert_eq!(info.len(), ix.facts().calls.len() as usize);
    let t3 = Instant::now();
    let mut stats = CallStats {
        calls: info.len() as u64,
        edges: edges.len() as u64,
        classes: u64::from(classes.len()),
        classes_time: t1 - t0,
        mro_time: t2 - t1,
        calls_time: t3 - t2,
        ..CallStats::default()
    };
    for &v in &info {
        stats.by_conf[(v >> 6) as usize] += 1;
        stats.by_reason[(v & 0x3f) as usize] += 1;
    }
    for e in &edges {
        stats.edges_by_reason[e.reason as usize] += 1;
    }
    for c in 0..classes.len() {
        let fl = classes.flags(c);
        stats.incomplete_classes += u64::from(fl & class_flags::INCOMPLETE != 0);
        stats.no_c3 += u64::from(fl & class_flags::NO_C3 != 0);
    }
    ResolvedCalls { classes, methods, info, edges, edge_start, stats }
}

/// Concatenates chunk outputs in file order.
fn merge_chunks(chunks: Vec<CallChunk>, files: usize) -> (Vec<u8>, Vec<CallEdge>, Vec<u32>) {
    let ni: usize = chunks.iter().map(|c| c.info.len()).sum();
    let ne: usize = chunks.iter().map(|c| c.edges.len()).sum();
    let mut info = Vec::with_capacity(ni);
    let mut edges = Vec::with_capacity(ne);
    let mut edge_start = Vec::with_capacity(files + 1);
    edge_start.push(0u32);
    for mut c in chunks {
        info.append(&mut c.info);
        edges.append(&mut c.edges);
        for l in c.edge_lens {
            let at = *edge_start.last().expect("non-empty") + l;
            edge_start.push(at);
        }
    }
    (info, edges, edge_start)
}

/// Output of the base pass for a chunk of files.
#[derive(Default)]
struct BaseChunk {
    bases: Vec<ClassId>,
    lens: Vec<u32>,
    flags: Vec<u8>,
}

/// Resolves the direct bases of every class (parallel per file), in the class's enclosing
/// scope. Returns the bases, their per-class starts and per-class flags.
fn class_bases(cx: &Cx<'_>) -> (Vec<ClassId>, Vec<u32>, Vec<u8>) {
    let ix = cx.ix;
    let files = ix.file_count() as usize;
    let chunks = par_chunks(files, |loc: &mut Locals, f, out: &mut BaseChunk| {
        let classes = cx.classes.file_classes(f);
        if classes.is_empty() {
            return;
        }
        let st = ix.facts();
        let fv = ix.file_view(f);
        let fnode = ix.file_node(f);
        // Local bindings only matter for classes defined inside functions or classes.
        if classes.clone().any(|c| cx.parent(f, cx.classes.node(c) - fnode) != 0) {
            loc.build(ix, f);
        }
        for c in classes {
            let d = cx.classes.node(c) - fnode;
            let scope = cx.parent(f, d);
            let before = out.bases.len();
            let mut fl = 0;
            for row in fv.bases(d) {
                if let Some(kw) = st.bases.keyword[row] {
                    if Some(kw) == cx.names.metaclass {
                        fl |= class_flags::METACLASS;
                    }
                    continue;
                }
                let chain = fv.base_chain(row);
                if chain.is_empty() || st.bases.flags[row] & base_flags::STARRED != 0 {
                    fl |= class_flags::INCOMPLETE;
                    continue;
                }
                let r = cx.chain(loc, f, scope, chain, 0);
                let last = chain.last().copied();
                match r.val {
                    Val::Def(v) if ix.kind(v) == NodeKind::Class => match cx.classes.id(ix, v) {
                        Some(b) if b != c => out.bases.push(b),
                        _ => fl |= class_flags::INCOMPLETE,
                    },
                    Val::Unknown(Reason::Builtin) if last == cx.names.object => {}
                    Val::Unknown(_) if last.is_some() && cx.names.markers.contains(&last) => {}
                    _ => fl |= class_flags::INCOMPLETE,
                }
            }
            out.lens.push((out.bases.len() - before) as u32);
            out.flags.push(fl);
        }
    });
    let n = cx.classes.len() as usize;
    let mut bases = Vec::with_capacity(chunks.iter().map(|c| c.bases.len()).sum());
    let mut start = Vec::with_capacity(n + 1);
    let mut flags = Vec::with_capacity(n);
    start.push(0u32);
    for mut c in chunks {
        bases.append(&mut c.bases);
        flags.append(&mut c.flags);
        for l in c.lens {
            let at = *start.last().expect("non-empty") + l;
            start.push(at);
        }
    }
    debug_assert_eq!(start.len(), n + 1);
    (bases, start, flags)
}

/// Per-task scratch of the call pass.
#[derive(Default)]
struct FileScratch {
    loc: Locals,
    linked: Vec<ClassId>,
}

/// Output of the call pass for a chunk of files.
#[derive(Default)]
struct CallChunk {
    info: Vec<u8>,
    edges: Vec<CallEdge>,
    edge_lens: Vec<u32>,
}

/// Where the edges of one site go.
struct Sink<'a> {
    src: NodeId,
    site: u32,
    out: &'a mut Vec<CallEdge>,
}

impl Sink<'_> {
    #[inline]
    fn push(&mut self, dst: NodeId, conf: Confidence, reason: Reason) {
        self.out.push(CallEdge { src: self.src, dst, site: self.site, conf, reason });
    }
}

/// One site to resolve.
struct Site<'a> {
    f: FileId,
    scope: u32,
    chain: &'a [Sym],
    recv: Receiver,
    lambda: bool,
}

/// Resolves the calls, uncalled decorators and overrides of file `f`.
fn file_calls(cx: &Cx<'_>, methods: &MethodIndex, sc: &mut FileScratch, f: FileId, out: &mut CallChunk) {
    let ix = cx.ix;
    let st = ix.facts();
    let ff = ix.file_facts(f);
    let fv = ix.file_view(f);
    let before = out.edges.len();
    if !ff.calls.is_empty() || !ff.decorators.is_empty() {
        sc.loc.build(ix, f);
    }
    let loc = &sc.loc;
    for row in ff.calls.range() {
        let scope = st.calls.scope[row];
        let mut sink = Sink { src: ix.def_node(f, scope), site: row as u32, out: &mut out.edges };
        let lambda = st.calls.flags[row] & call_flags::LAMBDA != 0;
        let site = Site { f, scope, chain: fv.call_chain(row), recv: st.calls.receiver(row), lambda };
        let (conf, why) = resolve_site(cx, methods, loc, &site, &mut sink, false);
        out.info.push(pack(conf, why));
    }
    if !ff.decorators.is_empty() {
        for d in 1..ff.defs.len {
            if st.defs.decorators_len[fv.def_row(d)] == 0 {
                continue;
            }
            let scope = cx.parent(f, d);
            for row in fv.decorators(d) {
                // `@deco(...)`: the factory call is a call row already.
                if st.decorators.flags[row] & dec_flags::CALLED != 0 {
                    continue;
                }
                let chain = fv.decorator_chain(row);
                if chain.is_empty() {
                    continue;
                }
                let mut sink = Sink { src: ix.def_node(f, scope), site: row as u32, out: &mut out.edges };
                let site = Site { f, scope, chain, recv: Receiver::Dotted, lambda: false };
                resolve_site(cx, methods, loc, &site, &mut sink, true);
            }
        }
    }
    override_edges(cx, f, &mut sc.linked, &mut out.edges);
    out.edge_lens.push((out.edges.len() - before) as u32);
}

/// Resolves one site, pushes its edges and returns its outcome. `decorator`: an uncalled
/// decorator (an edge to a function gets [`Reason::Decorator`]).
fn resolve_site(
    cx: &Cx<'_>,
    methods: &MethodIndex,
    loc: &Locals,
    s: &Site<'_>,
    sink: &mut Sink<'_>,
    decorator: bool,
) -> (Confidence, Reason) {
    let chain = s.chain;
    let r = match s.recv {
        Receiver::None | Receiver::Dotted => cx.chain(loc, s.f, s.scope, chain, 0),
        Receiver::SelfAttr | Receiver::ClsAttr => match cx.self_class(s.f, s.scope) {
            Some(c) if chain.len() >= 2 => {
                let instance = s.recv == Receiver::SelfAttr;
                let mut first = cx.class_attr(loc, c, chain[1], instance, 0, 0);
                if let Val::Def(_) = first.val {
                    first.why = if instance { Reason::SelfMethod } else { Reason::ClsMethod };
                } else if instance
                    && chain.len() == 2
                    && matches!(first.val, Val::Unknown(Reason::MissingAttr | Reason::ExternalBase))
                    && subclass_edges(cx, methods, c, chain[1], sink)
                {
                    return (Confidence::Likely, Reason::SubclassMethod);
                } else if chain.len() > 2 && first.val == Val::Unknown(Reason::MissingAttr) {
                    // `self.x.m()` with `x` set from elsewhere (a subclass, a caller).
                    first = R::unknown(Reason::Variable);
                }
                cx.attrs(loc, first, &chain[2..], 0)
            }
            Some(_) => R::unknown(Reason::UnknownReceiver),
            None => R::unknown(Reason::NoClass),
        },
        Receiver::Super => match (cx.self_class(s.f, s.scope), chain.split_first()) {
            (Some(c), Some((&m, rest))) => {
                let mut first = cx.class_attr(loc, c, m, true, 1, 0);
                if let Val::Def(_) = first.val {
                    first.why = Reason::SuperMethod;
                }
                cx.attrs(loc, first, rest, 0)
            }
            (Some(_), None) => R::unknown(Reason::UnknownReceiver),
            (None, _) => R::unknown(Reason::NoClass),
        },
        Receiver::Unknown => R::unknown(Reason::UnknownReceiver),
    };
    // Lambda parameters and comprehension targets are not recorded: a name may be one.
    let r = if s.lambda && !matches!(r.val, Val::Unknown(_)) { R { conf: r.conf.down(), ..r } } else { r };
    finish(cx, methods, loc, r, s, sink, decorator)
}

/// Turns the callee value into edges: a function, a class (constructor + `__init__` /
/// `__new__`), an instance (`__call__`), else the unique-method fallback.
fn finish(
    cx: &Cx<'_>,
    methods: &MethodIndex,
    loc: &Locals,
    r: R,
    s: &Site<'_>,
    sink: &mut Sink<'_>,
    decorator: bool,
) -> (Confidence, Reason) {
    let ix = cx.ix;
    match r.val {
        Val::Def(v) if ix.kind(v) == NodeKind::Class => {
            sink.push(v, r.conf, Reason::Constructor);
            if let Some(c) = cx.classes.id(ix, v) {
                for dunder in [cx.names.init, cx.names.new].into_iter().flatten() {
                    let a = cx.class_attr(loc, c, dunder, true, 0, 0);
                    if let Val::Def(t) = a.val
                        && ix.kind(t) == NodeKind::Function
                    {
                        sink.push(t, r.conf.max(a.conf), Reason::Init);
                    }
                }
            }
            (r.conf, Reason::Constructor)
        }
        Val::Def(v) => {
            let why = if decorator { Reason::Decorator } else { r.why };
            sink.push(v, r.conf, why);
            (r.conf, why)
        }
        Val::Instance(c) => {
            if let Some(call) = cx.names.call {
                let a = cx.class_attr(loc, c, call, true, 0, 0);
                if let Val::Def(t) = a.val
                    && ix.kind(t) == NodeKind::Function
                {
                    let conf = r.conf.max(a.conf).max(Confidence::Likely);
                    sink.push(t, conf, Reason::CallDunder);
                    return (conf, Reason::CallDunder);
                }
            }
            (Confidence::Unresolved, Reason::Variable)
        }
        Val::Module(_) => (Confidence::Unresolved, Reason::NotCallable),
        Val::Unknown(why) => {
            let attr_call = s.chain.len() >= 2 || (s.recv == Receiver::Unknown && !s.chain.is_empty());
            // `NotFound` heads are mostly comprehension targets and lambda parameters.
            let unknown = matches!(why, Reason::Parameter | Reason::Variable | Reason::UnknownReceiver | Reason::NotFound);
            if !attr_call || !unknown {
                return (Confidence::Unresolved, why);
            }
            let Some(&name) = s.chain.last() else { return (Confidence::Unresolved, why) };
            if cx.tables.builtin_methods.contains(name) {
                return (Confidence::Unresolved, Reason::BuiltinMethod);
            }
            match methods.candidates(name) {
                [] => (Confidence::Unresolved, why),
                [(_, t)] => {
                    sink.push(*t, Confidence::Possible, Reason::UniqueMethod);
                    (Confidence::Possible, Reason::UniqueMethod)
                }
                _ => (Confidence::Unresolved, Reason::Ambiguous),
            }
        }
    }
}

/// `self.m()` where class `c` has no `m`: edges to the methods `m` of its subclasses (only
/// when at most [`MAX_SUBCLASS_CANDIDATES`] methods have that name). Returns whether any
/// edge was added.
fn subclass_edges(cx: &Cx<'_>, methods: &MethodIndex, c: ClassId, name: Sym, sink: &mut Sink<'_>) -> bool {
    let cands = methods.candidates(name);
    if cands.len() > MAX_SUBCLASS_CANDIDATES {
        return false;
    }
    let mut any = false;
    for &(_, t) in cands {
        let Some(k) = cx.ix.parent(t).and_then(|p| cx.classes.id(cx.ix, p)) else { continue };
        if k != c && cx.classes.inherits(k, c) {
            sink.push(t, Confidence::Likely, Reason::SubclassMethod);
            any = true;
        }
    }
    any
}

/// Override liveness edges of the classes of file `f`: for every method, an edge from each
/// nearest overridden method in the MRO (a base method not already shadowed by a linked one,
/// so a diamond links both sides once).
fn override_edges(cx: &Cx<'_>, f: FileId, linked: &mut Vec<ClassId>, out: &mut Vec<CallEdge>) {
    let ix = cx.ix;
    let cl = cx.classes;
    for c in cl.file_classes(f) {
        let mro = cl.mro(c);
        if mro.len() < 2 {
            continue;
        }
        for m in cl.all_members(c) {
            if m.kind != mkind::DEF || ix.kind(m.r) != NodeKind::Function {
                continue;
            }
            linked.clear();
            for &k in &mro[1..] {
                let Some(b) = cl.members(k, m.name).first().filter(|b| b.kind == mkind::DEF) else { continue };
                if ix.kind(b.r) != NodeKind::Function {
                    continue;
                }
                if !linked.iter().any(|&l| cl.inherits(l, k)) {
                    out.push(CallEdge { src: b.r, dst: m.r, site: NONE, conf: Confidence::Likely, reason: Reason::Override });
                }
                linked.push(k);
            }
        }
    }
}
