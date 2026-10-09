//! The module table: every dotted package/module name of the index as a trie of name segments.

use crate::index::{FileId, Index, NONE, NodeId, NodeKind};
use crate::intern::{FxHashMap, Sym};
use crate::roots::FileStatus;

/// Index of a dotted name in a [`ModuleTable`].
pub type ModId = u32;

/// Parent of top-level names in the trie.
const TOP: u32 = u32::MAX;

/// How a dotted name is provided.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum ModKind {
    /// `name.py` / `name.pyi`.
    Module = 0,
    /// `name/__init__.py[i]`.
    Package = 1,
    /// A directory without `__init__` (PEP 420), in one or more distributions.
    Namespace = 2,
    /// Only non-importable or outside files carry the name: never an import target.
    Unavailable = 3,
}

/// Candidate rank of a package/module node for its dotted name (lower wins); `None` for nodes
/// that are never import targets. The implementation (`.py`, also when a stub shadows it) beats
/// a lone stub, which beats a namespace package, which beats a shadowed file.
fn rank(ix: &Index, v: NodeId) -> Option<u8> {
    let Some(f) = ix.file(v) else { return Some(2) };
    match ix.file_status(f) {
        FileStatus::Active if is_stub(ix, f) => Some(1),
        FileStatus::Active | FileStatus::StubShadowed => Some(0),
        FileStatus::Shadowed => Some(3),
        FileStatus::NotImportable | FileStatus::Outside => None,
    }
}

#[inline]
fn is_stub(ix: &Index, f: FileId) -> bool {
    use std::os::unix::ffi::OsStrExt;
    ix.file_path(f).as_os_str().as_bytes().ends_with(b".pyi")
}

#[inline]
fn key(parent: u32, seg: Sym) -> u64 {
    (u64::from(parent.wrapping_add(1)) << 32) | u64::from(seg.to_raw())
}

/// Every dotted package/module name of an [`Index`], deduplicated across distributions and
/// files, as a trie keyed by (parent, segment `Sym`): resolving `a.b.c` is three hash lookups
/// on `u64` keys, no string is built or hashed. Struct of arrays indexed by [`ModId`]
/// (17 bytes per name plus the child map).
///
/// Each name has one target node: the implementation (`.py` or `__init__.py`), else a lone
/// stub, else the namespace package, else a shadowed file; ties go to the lowest node (group
/// order, then file rank). A stub next to the implementation is kept as [`ModuleTable::stub`].
#[derive(Debug, Default)]
pub struct ModuleTable {
    /// Last segment.
    name: Vec<Sym>,
    /// Parent name (`NONE` for top-level names).
    parent: Vec<u32>,
    /// Target node (`NONE` when [`ModKind::Unavailable`]).
    node: Vec<NodeId>,
    /// Stub node next to the implementation (`NONE` if none, or if `node` is the stub).
    stub: Vec<NodeId>,
    kind: Vec<u8>,
    children: FxHashMap<u64, ModId>,
    /// Name of every file's package/module node (`NONE` if the file has no node).
    file_mod: Vec<ModId>,
}

impl ModuleTable {
    /// Builds the table from the hierarchy of `ix`: one pre-order pass over the package/module
    /// nodes (def subtrees are skipped in O(1)). O(packages + modules + top-level defs).
    pub fn build(ix: &Index) -> ModuleTable {
        let mut t = ModuleTable { file_mod: vec![NONE; ix.file_count() as usize], ..ModuleTable::default() };
        let mut ranks: Vec<u8> = Vec::new();
        // Open package/module nodes: (subtree end, name).
        let mut stack: Vec<(NodeId, ModId)> = Vec::new();
        let n = ix.node_count();
        let mut v = 0;
        while v < n {
            match ix.kind(v) {
                NodeKind::Distribution => {
                    stack.clear();
                    v += 1;
                }
                NodeKind::Class | NodeKind::Function => v = ix.subtree(v).end,
                NodeKind::Package | NodeKind::Module => {
                    while stack.last().is_some_and(|&(end, _)| end <= v) {
                        stack.pop();
                    }
                    let parent = stack.last().map_or(TOP, |&(_, m)| m);
                    let m = t.intern(parent, ix.name_sym(v), &mut ranks);
                    t.offer(ix, m, v, &mut ranks);
                    if let Some(f) = ix.file(v) {
                        t.file_mod[f as usize] = m;
                    }
                    stack.push((ix.subtree(v).end, m));
                    v += 1;
                }
            }
        }
        // A lone stub is the target itself.
        for (stub, &node) in t.stub.iter_mut().zip(&t.node) {
            if *stub == node {
                *stub = NONE;
            }
        }
        t.children.shrink_to_fit();
        t
    }

    /// The name `parent.seg`, added if new.
    fn intern(&mut self, parent: u32, seg: Sym, ranks: &mut Vec<u8>) -> ModId {
        let next = self.name.len() as ModId;
        let m = *self.children.entry(key(parent, seg)).or_insert(next);
        if m == next {
            self.name.push(seg);
            self.parent.push(if parent == TOP { NONE } else { parent });
            self.node.push(NONE);
            self.stub.push(NONE);
            self.kind.push(ModKind::Unavailable as u8);
            ranks.push(u8::MAX);
        }
        m
    }

    /// Offers node `v` as the target of name `m`.
    fn offer(&mut self, ix: &Index, m: ModId, v: NodeId, ranks: &mut [u8]) {
        let i = m as usize;
        let Some(r) = rank(ix, v) else { return };
        if r == 1 && self.stub[i] == NONE {
            self.stub[i] = v;
        }
        // Nodes come in increasing order, so the first of a rank wins ties.
        if r < ranks[i] {
            ranks[i] = r;
            self.node[i] = v;
            self.kind[i] = match (ix.kind(v), ix.file(v)) {
                (_, None) => ModKind::Namespace,
                (NodeKind::Package, _) => ModKind::Package,
                _ => ModKind::Module,
            } as u8;
        }
    }

    /// The table after a content-only update (same files, names and statuses): only the
    /// target and stub nodes move (`map` translates old nodes; package/module nodes survive).
    pub(super) fn remap_nodes(mut self, map: impl Fn(NodeId) -> NodeId) -> ModuleTable {
        for v in self.node.iter_mut().chain(self.stub.iter_mut()) {
            if *v != NONE {
                *v = map(*v);
            }
        }
        self
    }

    /// Number of names.
    #[inline]
    pub fn len(&self) -> u32 {
        self.name.len() as u32
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.name.is_empty()
    }

    /// Last segment of `m`.
    #[inline]
    pub fn name(&self, m: ModId) -> Sym {
        self.name[m as usize]
    }

    /// Parent package of `m` (`None` for top-level names).
    #[inline]
    pub fn parent(&self, m: ModId) -> Option<ModId> {
        let p = self.parent[m as usize];
        (p != NONE).then_some(p)
    }

    #[inline]
    pub fn kind(&self, m: ModId) -> ModKind {
        match self.kind[m as usize] {
            0 => ModKind::Module,
            1 => ModKind::Package,
            2 => ModKind::Namespace,
            _ => ModKind::Unavailable,
        }
    }

    /// Target node of `m` (package/module node); `None` when unavailable.
    #[inline]
    pub fn node(&self, m: ModId) -> Option<NodeId> {
        let v = self.node[m as usize];
        (v != NONE).then_some(v)
    }

    /// Stub node next to the implementation of `m`, if any.
    #[inline]
    pub fn stub(&self, m: ModId) -> Option<NodeId> {
        let v = self.stub[m as usize];
        (v != NONE).then_some(v)
    }

    /// The top-level ancestor of `m` (`m` itself if top-level).
    #[inline]
    pub fn top(&self, m: ModId) -> ModId {
        let mut cur = m;
        while let Some(p) = self.parent(cur) {
            cur = p;
        }
        cur
    }

    /// The name `m.seg` (`m = None`: the top-level name `seg`). One hash lookup.
    #[inline]
    pub fn child(&self, m: Option<ModId>, seg: Sym) -> Option<ModId> {
        self.children.get(&key(m.unwrap_or(TOP), seg)).copied()
    }

    /// Walks `segs` down from `from` (`None`: top level). Returns the deepest name reached and
    /// how many segments matched (`(from, 0)` when the first segment is unknown).
    pub fn walk(&self, from: Option<ModId>, segs: &[Sym]) -> (Option<ModId>, usize) {
        let mut cur = from;
        for (k, &s) in segs.iter().enumerate() {
            match self.child(cur, s) {
                Some(c) => cur = Some(c),
                None => return (cur, k),
            }
        }
        (cur, segs.len())
    }

    /// The name of a dotted string (`a.b.c`), if every segment is known.
    pub fn lookup(&self, ix: &Index, dotted: &[u8]) -> Option<ModId> {
        let mut cur = None;
        for seg in dotted.split(|&c| c == b'.') {
            cur = Some(self.child(cur, ix.strings().get(seg)?)?);
        }
        cur
    }

    /// Name of the package/module node of file `f` (`None` for files without one).
    #[inline]
    pub fn file_mod(&self, f: FileId) -> Option<ModId> {
        let m = self.file_mod[f as usize];
        (m != NONE).then_some(m)
    }

    /// Appends the dotted name of `m` to `out`.
    pub fn dotted(&self, ix: &Index, m: ModId, out: &mut Vec<u8>) {
        let start = out.len();
        let mut cur = Some(m);
        while let Some(c) = cur {
            if out.len() > start {
                out.push(b'.');
            }
            let s = ix.str(self.name(c));
            out.extend(s.iter().rev());
            cur = self.parent(c);
        }
        out[start..].reverse();
    }

    /// Heap bytes (capacity).
    pub fn heap_bytes(&self) -> usize {
        self.name.capacity() * 4
            + self.parent.capacity() * 4
            + self.node.capacity() * 4
            + self.stub.capacity() * 4
            + self.kind.capacity()
            + self.file_mod.capacity() * 4
            + self.children.capacity() * (size_of::<(u64, ModId)>() + 1)
    }
}
