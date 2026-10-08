//! Streaming loader and global symbol index (design: Loading / index).
//!
//! [`load`] turns a list of files ([`Inputs`]) into an [`Index`]:
//!
//! ```text
//!   Inputs (FileId order)          chunks of consecutive FileIds, in parallel (rayon)
//!   path, group, module name  -->  per worker: read into a reused buffer -> extract facts +
//!                                  metrics (worker interner) -> drop the source
//!                                  per chunk: rows moved into an exact-size FactStore
//!                              --> merge worker interners (Interner::merge_all), remap each
//!                                  chunk's Syms (parallel), concatenate chunks in chunk order
//!                              --> hierarchy (pre-order nodes), qualified ids, lookup tables
//! ```
//!
//! Global tables are struct-of-arrays:
//!
//! - files: path ([`PathList`]), group, status, dotted module name, [`FileFacts`] header,
//!   module node.
//! - facts: one merged [`FactStore`] (defs, imports, call sites, bindings, ...) plus the
//!   [`MetricsTable`]; rows of file `f` are the runs of its header, in FileId order. Scopes inside
//!   rows stay file-relative: def `d` of file `f` is node `file_node(f) + d`.
//! - nodes: the hierarchy distribution -> package -> module -> class -> function in pre-order.
//!
//! # Determinism
//!
//! Chunks hold consecutive FileIds and are concatenated in chunk order, so every table row,
//! node id and qualified id is the same whatever the thread count or scheduling. Only the
//! numeric values of [`Sym`]s depend on scheduling (which worker interned a string first):
//! compare and order by resolved bytes, never by `Sym`.
//!
//! # Hierarchy
//!
//! ```text
//!   node  kind          depth  subtree       qid
//!   0     Distribution  0      [0, 9)        @myproj
//!   1     Package       1      [1, 7)        pkg              pkg/__init__.py
//!   2     Function      2      [2, 3)        pkg:helper         its defs first
//!   3     Module        2      [3, 7)        pkg.mod          pkg/mod.py
//!   4     Class         3      [4, 7)        pkg.mod:Cls
//!   5     Function      4      [5, 6)        pkg.mod:Cls.run
//!   6     Function      4      [6, 7)        pkg.mod:Cls.run#2  (same name, same parent)
//!   7     Package       1      [7, 9)        tests            namespace (no file)
//!   8     Module        2      [8, 9)        tests.test_x
//! ```
//!
//! Children of a package: the defs of its `__init__` (source order), then sub-packages and
//! modules sorted by name. Distributions are sorted by (origin, name). Every node's subtree is
//! the range `v..end(v)`, so [`Index::is_descendant`] is two compares. [`Index::ancestor_at`]
//! is O(1): every node with children stores its ancestor path (depths `0..=depth`) once in a
//! pool, and a leaf reads its parent's path. The pool costs `depth + 1` entries per inner node
//! only (leaf functions, the bulk of the nodes, store nothing).
//!
//! # Qualified ids
//!
//! `pkg.mod:Class.method`: the dotted module (or package) name, `:`, then the def path. Stable
//! across runs and edits elsewhere (no byte offsets). Disambiguation, deterministic:
//!
//! - Defs: siblings with the same name get `#2`, `#3`, ... in source order (`@overload`s,
//!   property setters, conditional definitions).
//! - Modules and packages: the same dotted name in several files or distributions (a `.py`
//!   next to its `.pyi`, a module in two roots, a namespace package spread over
//!   distributions) gets `#2`, ... The implementation (`.py`) comes first, then stubs, then
//!   shadowed and non-importable files; ties by distribution order then FileId.
//! - Distributions: `@name`.
//!
//! Qids are rendered on demand ([`Index::qid`]) into a caller buffer; nothing is stored per
//! node beyond its name `Sym` and a `u16` duplicate ordinal.
//!
//! # Import roots seam
//!
//! [`Inputs`] is the only thing the loader knows about where files come from: per file a path,
//! a group (distribution node), a dotted module name and a [`FileStatus`]. It is built either
//! from one root directory ([`Inputs::from_root_dir`]) or from discovered import roots
//! ([`Inputs::from_roots`], PEP 420 / PEP 561 semantics of [`crate::roots::ModuleMap`]).

mod build;
mod inputs;
mod load;
mod qid;

#[cfg(test)]
mod tests;

pub use inputs::{Group, Inputs};
pub use load::{LoadStats, ReadError, load};

use std::ops::Range;
use std::path::Path;

use crate::extract::MetricsTable;
use crate::extract::facts::{FactStore, FileFacts, FileView};
use crate::intern::{FxHashMap, Interner, Sym};
use crate::roots::{FileStatus, Origin};
use crate::walk::PathList;

/// Index of a file in [`Inputs`] / [`Index`] (input order).
pub type FileId = u32;
/// Index of a node in pre-order.
pub type NodeId = u32;

/// "No node / no file" in `u32` columns.
pub const NONE: u32 = u32::MAX;

/// What a hierarchy node is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[repr(u8)]
pub enum NodeKind {
    Distribution = 0,
    /// A package: `__init__.py[i]`, or a directory (namespace package, no file).
    Package = 1,
    Module = 2,
    Class = 3,
    Function = 4,
}

impl NodeKind {
    #[inline]
    fn from_u8(v: u8) -> NodeKind {
        match v {
            0 => NodeKind::Distribution,
            1 => NodeKind::Package,
            2 => NodeKind::Module,
            3 => NodeKind::Class,
            _ => NodeKind::Function,
        }
    }

    /// A class or function (a row of the defs table other than `<module>`).
    #[inline]
    pub fn is_def(self) -> bool {
        matches!(self, NodeKind::Class | NodeKind::Function)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            NodeKind::Distribution => "distribution",
            NodeKind::Package => "package",
            NodeKind::Module => "module",
            NodeKind::Class => "class",
            NodeKind::Function => "function",
        }
    }
}

/// Hierarchy nodes, struct of arrays, in pre-order (25 bytes per node plus the ancestor pool).
#[derive(Default, Debug)]
pub(crate) struct Nodes {
    pub kind: Vec<u8>,
    /// Duplicate ordinal: 0 for the first of its name (qid suffix `#{dup + 1}` otherwise).
    pub dup: Vec<u16>,
    pub depth: Vec<u16>,
    /// `NONE` for distributions.
    pub parent: Vec<u32>,
    /// Subtree is `v..end`.
    pub end: Vec<u32>,
    pub name: Vec<Sym>,
    /// File of package/module/def nodes; `NONE` for distributions and namespace packages.
    pub file: Vec<u32>,
    /// Offset of the node's ancestor path in `Index::chain` (inner nodes only, else `NONE`).
    pub anc: Vec<u32>,
}

impl Nodes {
    #[inline]
    pub fn len(&self) -> u32 {
        self.kind.len() as u32
    }

    #[allow(clippy::too_many_arguments)]
    #[inline]
    pub fn push(&mut self, kind: NodeKind, dup: u16, depth: u16, parent: u32, name: Sym, file: u32) -> NodeId {
        let v = self.len();
        self.kind.push(kind as u8);
        self.dup.push(dup);
        self.depth.push(depth);
        self.parent.push(parent);
        self.end.push(v + 1);
        self.name.push(name);
        self.file.push(file);
        v
    }

    pub fn reserve(&mut self, n: usize) {
        self.kind.reserve_exact(n);
        self.dup.reserve_exact(n);
        self.depth.reserve_exact(n);
        self.parent.reserve_exact(n);
        self.end.reserve_exact(n);
        self.name.reserve_exact(n);
        self.file.reserve_exact(n);
    }

    pub fn heap_bytes(&self) -> usize {
        self.kind.capacity()
            + 2 * (self.dup.capacity() + self.depth.capacity())
            + 4 * (self.parent.capacity()
                + self.end.capacity()
                + self.name.capacity()
                + self.file.capacity()
                + self.anc.capacity())
    }
}

/// Per-file table, struct of arrays, indexed by [`FileId`].
#[derive(Default, Debug)]
pub(crate) struct Files {
    pub facts: Vec<FileFacts>,
    pub group: Vec<u32>,
    pub status: Vec<FileStatus>,
    /// Dotted module name (in [`Index::strings`]).
    pub module: Vec<Sym>,
    /// The file's package/module node (def `d` of the file is node `node + d`).
    pub node: Vec<NodeId>,
}

/// The loaded program: files, facts, metrics and the hierarchy. See the module docs.
pub struct Index {
    /// Every string of the facts, module names and node names.
    strings: Interner,
    paths: PathList,
    groups: Vec<Group>,
    files: Files,
    facts: FactStore,
    metrics: MetricsTable,
    nodes: Nodes,
    /// Ancestor paths of inner nodes (see [`Nodes::anc`]).
    chain: Vec<u32>,
    /// Distribution node per group.
    group_node: Vec<NodeId>,
    /// (dotted name, duplicate ordinal) -> package/module node.
    modules: FxHashMap<(Sym, u16), NodeId>,
    read_errors: Vec<ReadError>,
    stats: LoadStats,
}

impl Index {
    /// The interner of every `Sym` in the index.
    #[inline]
    pub fn strings(&self) -> &Interner {
        &self.strings
    }

    /// Text of a `Sym` of this index.
    #[inline]
    pub fn str(&self, s: Sym) -> &[u8] {
        self.strings.resolve(s)
    }

    /// The merged fact tables (rows of every file, in FileId order).
    #[inline]
    pub fn facts(&self) -> &FactStore {
        &self.facts
    }

    /// Metrics of every def row except `<module>` rows; see [`Index::metrics_row`].
    #[inline]
    pub fn metrics(&self) -> &MetricsTable {
        &self.metrics
    }

    pub fn groups(&self) -> &[Group] {
        &self.groups
    }

    pub fn read_errors(&self) -> &[ReadError] {
        &self.read_errors
    }

    pub fn stats(&self) -> &LoadStats {
        &self.stats
    }

    // -- files ----------------------------------------------------------------------------

    #[inline]
    pub fn file_count(&self) -> u32 {
        self.files.facts.len() as u32
    }

    #[inline]
    pub fn file_path(&self, f: FileId) -> &Path {
        self.paths.get(f as usize)
    }

    /// Fact header of a file (store-absolute runs into [`Index::facts`]).
    #[inline]
    pub fn file_facts(&self, f: FileId) -> &FileFacts {
        &self.files.facts[f as usize]
    }

    /// Read access to one file's facts.
    #[inline]
    pub fn file_view(&self, f: FileId) -> FileView<'_> {
        self.facts.file(&self.files.facts[f as usize])
    }

    #[inline]
    pub fn file_group(&self, f: FileId) -> u32 {
        self.files.group[f as usize]
    }

    #[inline]
    pub fn file_origin(&self, f: FileId) -> Origin {
        self.groups[self.files.group[f as usize] as usize].origin
    }

    #[inline]
    pub fn file_status(&self, f: FileId) -> FileStatus {
        self.files.status[f as usize]
    }

    /// Dotted module name of a file (shadowed and non-importable files included).
    #[inline]
    pub fn file_module(&self, f: FileId) -> Sym {
        self.files.module[f as usize]
    }

    /// The package/module node of a file (its `<module>` def row).
    #[inline]
    pub fn file_node(&self, f: FileId) -> NodeId {
        self.files.node[f as usize]
    }

    /// Node of file-relative def `d` (a fact `scope`) of file `f`.
    #[inline]
    pub fn def_node(&self, f: FileId, d: u32) -> NodeId {
        debug_assert!(d < self.files.facts[f as usize].defs.len);
        self.files.node[f as usize] + d
    }

    // -- nodes ----------------------------------------------------------------------------

    #[inline]
    pub fn node_count(&self) -> u32 {
        self.nodes.len()
    }

    #[inline]
    pub fn kind(&self, v: NodeId) -> NodeKind {
        NodeKind::from_u8(self.nodes.kind[v as usize])
    }

    /// Depth: 0 for distributions.
    #[inline]
    pub fn depth(&self, v: NodeId) -> u32 {
        u32::from(self.nodes.depth[v as usize])
    }

    #[inline]
    pub fn parent(&self, v: NodeId) -> Option<NodeId> {
        let p = self.nodes.parent[v as usize];
        (p != NONE).then_some(p)
    }

    /// Simple name: last dotted segment for packages/modules, the group name for distributions.
    #[inline]
    pub fn name(&self, v: NodeId) -> &[u8] {
        self.strings.resolve(self.nodes.name[v as usize])
    }

    #[inline]
    pub fn name_sym(&self, v: NodeId) -> Sym {
        self.nodes.name[v as usize]
    }

    /// Duplicate ordinal (0 = first of its name; see the module docs).
    #[inline]
    pub fn dup(&self, v: NodeId) -> u16 {
        self.nodes.dup[v as usize]
    }

    /// File of a package/module/def node.
    #[inline]
    pub fn file(&self, v: NodeId) -> Option<FileId> {
        let f = self.nodes.file[v as usize];
        (f != NONE).then_some(f)
    }

    /// The node and all its descendants: `v..end`.
    #[inline]
    pub fn subtree(&self, v: NodeId) -> Range<NodeId> {
        v..self.nodes.end[v as usize]
    }

    /// `d` is `a` or below it. O(1).
    #[inline]
    pub fn is_descendant(&self, a: NodeId, d: NodeId) -> bool {
        a <= d && d < self.nodes.end[a as usize]
    }

    /// The ancestor of `v` at depth `k` (`v` itself when `k == depth(v)`). O(1).
    ///
    /// # Panics
    /// If `k > depth(v)`.
    #[inline]
    pub fn ancestor_at(&self, v: NodeId, k: u32) -> NodeId {
        let d = self.depth(v);
        assert!(k <= d, "ancestor_at: depth {k} below node depth {d}");
        if k == d {
            return v;
        }
        let p = self.nodes.parent[v as usize];
        self.chain[(self.nodes.anc[p as usize] + k) as usize]
    }

    /// Direct children of `v`, in order.
    #[inline]
    pub fn children(&self, v: NodeId) -> Children<'_> {
        Children { end: &self.nodes.end, next: v + 1, stop: self.nodes.end[v as usize] }
    }

    /// Distribution node of a group.
    #[inline]
    pub fn group_node(&self, g: u32) -> NodeId {
        self.group_node[g as usize]
    }

    /// Global row of a node in the defs table of [`Index::facts`] (the `<module>` row for
    /// package/module nodes with a file); `None` for distributions and namespace packages.
    #[inline]
    pub fn def_row(&self, v: NodeId) -> Option<usize> {
        let f = self.file(v)?;
        let ff = &self.files.facts[f as usize];
        Some((ff.defs.start + (v - self.files.node[f as usize])) as usize)
    }

    /// Row of a class/function node in [`Index::metrics`].
    #[inline]
    pub fn metrics_row(&self, v: NodeId) -> Option<usize> {
        let f = self.file(v)?;
        let rel = v - self.files.node[f as usize];
        if rel == 0 {
            return None;
        }
        // One metrics row per def row except `<module>`, files in FileId order.
        Some((self.files.facts[f as usize].defs.start - f + rel - 1) as usize)
    }

    /// Heap bytes held by the index (capacity): interner, paths, facts, metrics, nodes and
    /// lookup tables.
    pub fn heap_bytes(&self) -> usize {
        self.strings.heap_bytes()
            + self.paths.byte_len()
            + self.paths.len() * 4
            + self.files.facts.capacity() * size_of::<FileFacts>()
            + self.files.group.capacity() * 4
            + self.files.status.capacity()
            + self.files.module.capacity() * 4
            + self.files.node.capacity() * 4
            + self.facts.heap_bytes()
            + self.metrics.heap_bytes()
            + self.nodes.heap_bytes()
            + self.chain.capacity() * 4
            + self.group_node.capacity() * 4
            + self.modules.capacity() * (size_of::<((Sym, u16), NodeId)>() + 1)
    }
}

/// Iterator over the direct children of a node (skips each child's subtree in O(1)).
#[derive(Clone)]
pub struct Children<'a> {
    end: &'a [u32],
    next: u32,
    stop: u32,
}

impl Iterator for Children<'_> {
    type Item = NodeId;

    #[inline]
    fn next(&mut self) -> Option<NodeId> {
        if self.next >= self.stop {
            return None;
        }
        let c = self.next;
        self.next = self.end[c as usize];
        Some(c)
    }
}
