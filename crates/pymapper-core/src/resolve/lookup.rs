//! Name and attribute lookup of the call resolver: lexical scopes (a per-file table of local
//! bindings), module namespaces, builtins, class MROs and construction hints.

use super::calls::{Confidence, Names, Reason, Tables};
use super::classes::{ClassId, Classes, Member, class_flags, mkind};
use super::modules::ModId;
use super::namespace::kind as nskind;
use super::{Global, Imports};
use crate::extract::events::AssignKind;
use crate::extract::facts::{DefKind, ValueKind, assign_kind, binding_flags, decl_kind, import_flags};
use crate::index::{FileId, Index, NodeId, NodeKind};
use crate::intern::Sym;

/// Kinds of [`Local`] rows, in lookup priority order.
mod lkind {
    pub const GLOBAL: u8 = 0;
    pub const NONLOCAL: u8 = 1;
    pub const DEF: u8 = 2;
    pub const IMPORT: u8 = 3;
    pub const VAR: u8 = 4;
    pub const PARAM: u8 = 5;
}

/// A name bound in a function or class scope (16 bytes).
#[derive(Clone, Copy, Debug)]
struct Local {
    scope: u32,
    name: Sym,
    /// Def row (file-relative) for `DEF`, else a store-absolute row of the bindings, imports
    /// or params table.
    r: u32,
    kind: u8,
}

/// The local bindings of one file, sorted by (scope, name, kind, row). Rebuilt per file in a
/// reused buffer.
#[derive(Default)]
pub(super) struct Locals {
    rows: Vec<Local>,
    /// The file the rows belong to (`NONE`: none yet).
    f: FileId,
    built: bool,
}

impl Locals {
    /// Rebuilds the table for file `f`.
    pub fn build(&mut self, ix: &Index, f: FileId) {
        self.rows.clear();
        self.f = f;
        self.built = true;
        let st = ix.facts();
        let ff = ix.file_facts(f);
        let fv = ix.file_view(f);
        let d0 = ff.defs.start as usize;
        for row in ff.defs.range().skip(1) {
            let d = (row - d0) as u32;
            let p = st.defs.parent[row];
            if p != 0
                && let Some(name) = st.defs.name[row]
            {
                self.rows.push(Local { scope: p, name, r: d, kind: lkind::DEF });
            }
            if st.defs.kind[row] == DefKind::Function as u8 {
                for pr in fv.params(d) {
                    self.rows.push(Local { scope: d, name: st.params.name[pr], r: pr as u32, kind: lkind::PARAM });
                }
            }
        }
        for row in ff.bindings.range() {
            let s = st.bindings.scope[row];
            if s != 0 && st.bindings.target_len[row] == 1 {
                self.rows.push(Local { scope: s, name: st.bindings.target[row], r: row as u32, kind: lkind::VAR });
            }
        }
        for row in ff.imports.range() {
            let s = st.imports.scope[row];
            let fl = st.imports.flags[row];
            if s == 0 || fl & import_flags::STAR != 0 {
                continue;
            }
            let bound = match (st.imports.alias[row], st.imports.name[row]) {
                (Some(a), _) => Some(a),
                (None, Some(n)) if fl & import_flags::FROM != 0 => Some(n),
                _ if fl & import_flags::FROM == 0 => fv.import_module(row).first().copied(),
                _ => None,
            };
            if let Some(name) = bound {
                self.rows.push(Local { scope: s, name, r: row as u32, kind: lkind::IMPORT });
            }
        }
        for row in ff.decls.range() {
            let kind = match decl_kind(st.decls.kind[row]) {
                crate::extract::events::DeclKind::Global => lkind::GLOBAL,
                crate::extract::events::DeclKind::Nonlocal => lkind::NONLOCAL,
            };
            self.rows.push(Local { scope: st.decls.scope[row], name: st.decls.name[row], r: row as u32, kind });
        }
        self.rows.sort_unstable_by_key(|l| (l.scope, l.name.to_raw(), l.kind, l.r));
    }

    /// Whether the table holds the bindings of file `f`.
    #[inline]
    pub fn is_for(&self, f: FileId) -> bool {
        self.built && self.f == f
    }

    /// Bindings of `name` in `scope` (kind order).
    #[inline]
    fn get(&self, scope: u32, name: Sym) -> &[Local] {
        let key = (scope, name.to_raw());
        let i = self.rows.partition_point(|l| (l.scope, l.name.to_raw()) < key);
        let j = i + self.rows[i..].partition_point(|l| (l.scope, l.name.to_raw()) == key);
        &self.rows[i..j]
    }
}

/// A resolved value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Val {
    /// A class or function node.
    Def(NodeId),
    Module(ModId),
    /// An instance of a loaded class.
    Instance(ClassId),
    /// Nothing loaded; why.
    Unknown(Reason),
}

/// A value with how it was reached.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct R {
    pub val: Val,
    pub conf: Confidence,
    pub why: Reason,
}

impl R {
    #[inline]
    pub fn unknown(why: Reason) -> R {
        R { val: Val::Unknown(why), conf: Confidence::Unresolved, why }
    }

    #[inline]
    fn new(val: Val, conf: Confidence, why: Reason) -> R {
        R { val, conf, why }
    }

    /// The worse of the two confidences.
    #[inline]
    fn worst(mut self, c: Confidence) -> R {
        if !matches!(self.val, Val::Unknown(_)) {
            self.conf = self.conf.max(c);
        }
        self
    }
}

/// Deepest chain of hints / aliases / re-exported variables followed.
const MAX_DEPTH: u8 = 8;

/// Read-only context of the call resolver.
pub(super) struct Cx<'a> {
    pub ix: &'a Index,
    pub imp: &'a Imports,
    pub classes: &'a Classes,
    pub tables: &'a Tables,
    pub names: Names,
}

impl Cx<'_> {
    #[inline]
    fn def_kind(&self, f: FileId, d: u32) -> u8 {
        self.ix.facts().defs.kind[self.ix.file_view(f).def_row(d)]
    }

    #[inline]
    pub fn parent(&self, f: FileId, d: u32) -> u32 {
        self.ix.facts().defs.parent[self.ix.file_view(f).def_row(d)]
    }

    /// Class id of file-relative class def `d` of `f`.
    #[inline]
    fn class_of_def(&self, f: FileId, d: u32) -> Option<ClassId> {
        self.classes.id(self.ix, self.ix.def_node(f, d))
    }

    /// The class whose method encloses `scope` (through nested functions): what `self`,
    /// `cls` and `super()` refer to.
    pub fn self_class(&self, f: FileId, scope: u32) -> Option<ClassId> {
        let mut s = scope;
        while s != 0 {
            if self.def_kind(f, s) != DefKind::Function as u8 {
                return None;
            }
            let p = self.parent(f, s);
            if p != 0 && self.def_kind(f, p) == DefKind::Class as u8 {
                return self.class_of_def(f, p);
            }
            s = p;
        }
        None
    }

    /// Resolves the value of a dotted chain evaluated in `scope` of file `f`.
    pub fn chain(&self, loc: &Locals, f: FileId, scope: u32, chain: &[Sym], depth: u8) -> R {
        let Some((&first, rest)) = chain.split_first() else { return R::unknown(Reason::UnknownReceiver) };
        let r = self.name(loc, f, scope, first, depth);
        self.attrs(loc, r, rest, depth)
    }

    /// Applies attribute steps `segs` to `r`.
    pub fn attrs(&self, loc: &Locals, mut r: R, segs: &[Sym], depth: u8) -> R {
        for (i, &seg) in segs.iter().enumerate() {
            if matches!(r.val, Val::Unknown(_)) {
                break;
            }
            let on_object = matches!(r.val, Val::Instance(_) | Val::Def(_));
            r = self.attr(loc, r, seg, depth);
            if on_object && i + 1 < segs.len() && r.val == Val::Unknown(Reason::MissingAttr) {
                // An attribute set from elsewhere (a subclass, a caller): its value is unknown.
                r = R::unknown(Reason::Variable);
            }
        }
        r
    }

    /// Looks up a bare name from `scope` of file `f`: the scope itself, the enclosing function
    /// scopes (class bodies only when the lookup starts in them), the module, the builtins.
    /// The local bindings table is used when `loc` holds `f`; otherwise function scopes are
    /// skipped (class bodies are looked up through the class table).
    pub fn name(&self, loc: &Locals, f: FileId, scope: u32, name: Sym, depth: u8) -> R {
        let have = loc.is_for(f);
        let mut s = scope;
        let mut first = true;
        while s != 0 {
            let is_class = self.def_kind(f, s) == DefKind::Class as u8;
            if !is_class || first {
                if have {
                    let sl = loc.get(s, name);
                    if !sl.is_empty() {
                        match sl[0].kind {
                            lkind::GLOBAL => return self.global(loc, f, name, depth, true),
                            lkind::NONLOCAL => {}
                            _ => return self.local(loc, f, sl, depth),
                        }
                    }
                } else if is_class
                    && let Some(c) = self.class_of_def(f, s)
                    && let Some(r) = self.own_member(loc, c, name, depth)
                {
                    return r;
                }
            }
            first = false;
            s = self.parent(f, s);
        }
        self.global(loc, f, name, depth, true)
    }

    /// A class-body name looked up through the class table (no locals): a def or assignment
    /// of the class itself.
    fn own_member(&self, loc: &Locals, c: ClassId, name: Sym, depth: u8) -> Option<R> {
        let ms = self.classes.members(c, name);
        let m = ms.iter().find(|m| m.kind != mkind::IATTR)?;
        Some(self.member(loc, c, *m, depth))
    }

    /// The value of a name bound in a function / class scope (`sl`: its bindings there).
    fn local(&self, loc: &Locals, f: FileId, sl: &[Local], depth: u8) -> R {
        let e = sl[0];
        let mixed = sl.len() > 1 && sl[1..].iter().any(|o| !self.same_binding(f, e, *o));
        let r = match e.kind {
            lkind::DEF => R::new(Val::Def(self.ix.def_node(f, e.r)), Confidence::Certain, Reason::Local),
            lkind::IMPORT => self.import_row(loc, e.r as usize, depth),
            lkind::VAR => self.binding(loc, f, e.r as usize, depth, Reason::LocalHint),
            _ => return R::unknown(Reason::Parameter),
        };
        if mixed { r.worst(r.conf.down()) } else { r }
    }

    /// Whether two bindings of a name in one scope bind the same thing (same kind; for
    /// assignments, the same value shape and chain).
    fn same_binding(&self, f: FileId, a: Local, b: Local) -> bool {
        if a.kind != b.kind {
            return false;
        }
        if a.kind != lkind::VAR {
            return true;
        }
        let st = &self.ix.facts().bindings;
        let (ra, rb) = (a.r as usize, b.r as usize);
        let fv = self.ix.file_view(f);
        st.value_kind(ra) == st.value_kind(rb) && fv.binding_value(ra) == fv.binding_value(rb)
    }

    /// A module-level name of file `f`: its namespace (defs, imports, assignments), its star
    /// imports, then (`builtins`) the builtins.
    pub fn global(&self, loc: &Locals, f: FileId, name: Sym, depth: u8, builtins: bool) -> R {
        let ix = self.ix;
        match self.imp.ns.get(f, name) {
            Some(e) if e.base() == nskind::DEF => {
                return R::new(Val::Def(ix.def_node(f, e.r)), Confidence::Certain, Reason::Global);
            }
            Some(e) if e.base() == nskind::VAR => return self.binding(loc, f, e.r as usize, depth, Reason::GlobalHint),
            Some(e) if e.base() == nskind::IMPORT => return self.import_row(loc, e.r as usize, depth),
            _ => {}
        }
        let is_builtin = builtins && self.tables.builtins.contains(name);
        match self.imp.global(ix, f, name) {
            // A star import of unresolved code could bind anything; builtins win.
            Some(Global::Dynamic) if is_builtin => return R::unknown(Reason::Builtin),
            Some(g) => return self.global_val(g),
            None => {}
        }
        if is_builtin {
            return R::unknown(Reason::Builtin);
        }
        if self.imp.ns.is_dynamic(f) {
            return R::unknown(Reason::Dynamic);
        }
        R::unknown(Reason::NotFound)
    }

    /// What import `row` binds its name to. A re-exported module variable is followed to its
    /// binding (for construction hints).
    fn import_row(&self, loc: &Locals, row: usize, depth: u8) -> R {
        let g = self.imp.bound_by(self.ix, row);
        if let Global::Variable(v) = g
            && depth < MAX_DEPTH
        {
            let st = self.ix.facts();
            let name = st.imports.name[row].filter(|_| st.imports.flags[row] & import_flags::FROM != 0);
            if let (Some(name), Some(file)) = (name, self.ix.file(v)) {
                let r = self.global(loc, file, name, depth + 1, false);
                return if matches!(r.val, Val::Unknown(Reason::NotFound)) { R::unknown(Reason::Variable) } else { r };
            }
        }
        self.global_val(g)
    }

    fn global_val(&self, g: Global) -> R {
        match g {
            Global::Def(v) => R::new(Val::Def(v), Confidence::Certain, Reason::Imported),
            Global::Module(v) => match self.node_mod(v) {
                Some(m) => R::new(Val::Module(m), Confidence::Certain, Reason::Imported),
                None => R::unknown(Reason::Dynamic),
            },
            Global::Variable(_) => R::unknown(Reason::Variable),
            Global::Unresolved(row) => {
                let st = self.imp.status(row as usize);
                if st.is_external() { R::unknown(Reason::External) } else { R::unknown(Reason::UnresolvedImport) }
            }
            Global::Dynamic => R::unknown(Reason::Dynamic),
        }
    }

    /// The module name of a package/module node (namespace packages: through their dotted
    /// path, without allocating).
    fn node_mod(&self, v: NodeId) -> Option<ModId> {
        let ix = self.ix;
        let mods = &self.imp.modules;
        if let Some(f) = ix.file(v) {
            return mods.file_mod(f);
        }
        if !matches!(ix.kind(v), NodeKind::Package | NodeKind::Module) {
            return None;
        }
        let mut cur = None;
        for k in 1..=ix.depth(v) {
            cur = Some(mods.child(cur, ix.name_sym(ix.ancestor_at(v, k)))?);
        }
        cur
    }

    /// The value an assignment binds (binding row `brow` of file `f`): an instance of the
    /// class a construction calls (`why` = the hint kind), the target of an alias, or nothing
    /// usable ([`Reason::Variable`]).
    pub fn binding(&self, loc: &Locals, f: FileId, brow: usize, depth: u8, why: Reason) -> R {
        if depth >= MAX_DEPTH {
            return R::unknown(Reason::Variable);
        }
        let ix = self.ix;
        let b = &ix.facts().bindings;
        let plain = matches!(assign_kind(b.kind[brow]), AssignKind::Plain | AssignKind::Annotated | AssignKind::Walrus);
        let single = b.flags[brow] & (binding_flags::UNPACKED | binding_flags::STARRED) == 0;
        if !plain || !single {
            return R::unknown(Reason::Variable);
        }
        let vk = b.value_kind(brow);
        if !matches!(vk, ValueKind::Call | ValueKind::Name) {
            return R::unknown(Reason::Variable);
        }
        let scope = b.scope[brow];
        let chain = ix.file_view(f).binding_value(brow);
        let r = self.chain(loc, f, scope, chain, depth + 1);
        match (vk, r.val) {
            (_, Val::Unknown(Reason::External | Reason::Builtin | Reason::Dynamic)) => r,
            (ValueKind::Call, Val::Def(v)) if ix.kind(v) == NodeKind::Class => match self.classes.id(ix, v) {
                Some(c) => R::new(Val::Instance(c), r.conf.max(Confidence::Likely), why),
                None => R::unknown(Reason::Variable),
            },
            (ValueKind::Name, Val::Def(_) | Val::Module(_)) => R::new(r.val, r.conf.max(Confidence::Likely), Reason::Alias),
            (ValueKind::Name, Val::Instance(_)) => r,
            _ => R::unknown(Reason::Variable),
        }
    }

    /// Attribute `seg` of `r`.
    fn attr(&self, loc: &Locals, r: R, seg: Sym, depth: u8) -> R {
        let ix = self.ix;
        match r.val {
            Val::Module(m) => {
                let mut a = self.module_attr(loc, m, seg, depth);
                if matches!(a.val, Val::Def(_) | Val::Module(_)) {
                    a.why = Reason::ModuleAttr;
                }
                a.worst(r.conf)
            }
            Val::Def(v) if ix.kind(v) == NodeKind::Class => match self.classes.id(ix, v) {
                Some(c) => self.class_attr(loc, c, seg, false, 0, depth).worst(r.conf),
                None => R::unknown(Reason::MissingAttr),
            },
            Val::Def(_) => R::unknown(Reason::FunctionAttr),
            Val::Instance(c) => {
                let mut a = self.class_attr(loc, c, seg, true, 0, depth);
                if matches!(a.val, Val::Def(_)) {
                    a.why = r.why;
                }
                a.worst(r.conf)
            }
            Val::Unknown(_) => r,
        }
    }

    /// Attribute `seg` of module `m`: a binding of its file, else submodules, star imports
    /// and stubs (`from m import seg` semantics).
    fn module_attr(&self, loc: &Locals, m: ModId, seg: Sym, depth: u8) -> R {
        let ix = self.ix;
        if let Some(file) = self.imp.modules.node(m).and_then(|v| ix.file(v))
            && let Some(e) = self.imp.ns.get(file, seg)
            && matches!(e.base(), nskind::DEF | nskind::VAR | nskind::IMPORT)
        {
            return self.global(loc, file, seg, depth + 1, false);
        }
        match self.imp.attribute(ix, m, seg) {
            Some(g) => self.global_val(g),
            None => R::unknown(Reason::MissingAttr),
        }
    }

    /// Attribute `seg` of class `c` (`instance`: of an instance of it) through its MRO,
    /// skipping the first `skip` classes (`super()`: 1). A member found after the first class
    /// of an incomplete MRO is only `Likely` (an unloaded base may come first).
    pub fn class_attr(&self, loc: &Locals, c: ClassId, seg: Sym, instance: bool, skip: usize, depth: u8) -> R {
        let cl = self.classes;
        for (i, &k) in cl.mro(c).iter().enumerate().skip(skip) {
            let Some(m) = pick(cl.members(k, seg), instance) else { continue };
            let r = self.member(loc, k, m, depth);
            return if i > 0 && cl.flags(c) & class_flags::INCOMPLETE != 0 { r.worst(Confidence::Likely) } else { r };
        }
        if instance && Some(seg) == self.names.class_ {
            // `self.__class__(...)`: the class itself, or a subclass.
            return R::new(Val::Def(cl.node(c)), Confidence::Likely, Reason::ClassAttr);
        }
        let fl = cl.flags(c);
        if fl & class_flags::GETATTR != 0 {
            R::unknown(Reason::Dynamic)
        } else if fl & class_flags::INCOMPLETE != 0 || (!instance && fl & class_flags::METACLASS != 0) {
            R::unknown(Reason::ExternalBase)
        } else if is_dunder(self.ix.str(seg)) {
            // Inherited from `object` (`__init__`, `__repr__`, `__init_subclass__`, ...).
            R::unknown(Reason::Builtin)
        } else {
            R::unknown(Reason::MissingAttr)
        }
    }

    /// The value of member `m` of class `k`.
    fn member(&self, loc: &Locals, k: ClassId, m: Member, depth: u8) -> R {
        if m.kind == mkind::DEF {
            return R::new(Val::Def(m.r), Confidence::Certain, Reason::ClassAttr);
        }
        match self.ix.file(self.classes.node(k)) {
            Some(file) => self.binding(loc, file, m.r as usize, depth + 1, Reason::AttrHint),
            None => R::unknown(Reason::Variable),
        }
    }
}

/// `__name__`.
#[inline]
fn is_dunder(s: &[u8]) -> bool {
    s.len() > 4 && s.starts_with(b"__") && s.ends_with(b"__")
}

/// The member a lookup uses among the bindings of one name in one class: a def, else (on
/// instances) an instance attribute, else a class attribute.
#[inline]
fn pick(ms: &[Member], instance: bool) -> Option<Member> {
    let mut var = None;
    for &m in ms {
        match m.kind {
            mkind::DEF => return Some(m),
            mkind::IATTR if instance => return Some(m),
            mkind::VAR => var = Some(m),
            _ => {}
        }
    }
    var
}
