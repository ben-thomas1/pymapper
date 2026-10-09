//! The index diff of an update: node correspondence, added / removed / changed nodes.
//!
//! Before the old index is patched, [`OldSnap::take`] keeps the little the diff needs from it:
//! the node of every old file, the defs of the files that are re-extracted or removed (parent,
//! name, duplicate ordinal and signature hashes), the qualified ids of those defs and of
//! the nodes without a file (distributions, namespace packages), and, when module names or
//! statuses changed, the duplicate ordinal and parent of every node. Old nodes are then matched:
//!
//! - nodes of kept files: by offset from the file node (their rows did not change);
//! - defs of re-extracted files: by (matched parent, name, duplicate ordinal), top-down, so a
//!   def keeps its identity when code above it moves or changes;
//! - distributions and namespace packages, and the nodes of removed files: by qualified id,
//!   to a node not matched by the rules above (a package whose `__init__.py` is removed lives
//!   on as a namespace package; a module turned into a package keeps its defs).

use std::hash::Hasher;

use crate::index::{FileId, Index, NONE, NodeId, Src};
use crate::intern::{FxHashMap, FxHasher, Sym};

/// What an update changed, for callers that keep state keyed by node (the GUI).
///
/// Apply it as: translate every old node with [`IndexDiff::node_map`] (`NONE`: gone), then
/// add [`IndexDiff::added`], refresh [`IndexDiff::changed`] and apply the edge deltas. Edges
/// with an endpoint that is gone are not listed (they are gone with it).
#[derive(Debug, Default)]
pub struct IndexDiff {
    /// New node of every old node (`NONE`: removed).
    pub node_map: Vec<NodeId>,
    /// New nodes without an old counterpart (new ids, ascending).
    pub added: Vec<NodeId>,
    /// Old nodes without a new counterpart (old ids, ascending); see [`IndexDiff::removed_qid`].
    pub removed: Vec<NodeId>,
    /// Matched nodes whose facts changed: kind, flags, decorators, bases, parameters,
    /// metrics, duplicate ordinal, parent or file; the package/module node of every
    /// re-extracted file (new ids, ascending).
    pub changed: Vec<NodeId>,
    /// Matched defs of re-extracted files that only moved: same facts, other lines or byte
    /// span (new ids, ascending; disjoint from `changed`).
    pub moved: Vec<NodeId>,
    /// Files whose facts were re-extracted (new ids, ascending).
    pub reextracted: Vec<FileId>,
    /// Files of the new inputs that were not loaded before (new ids, ascending).
    pub added_files: Vec<FileId>,
    /// Files of the old index that are gone (old ids, ascending).
    pub removed_files: Vec<FileId>,
    /// Module-level import edges (file node -> package/module node) that appeared,
    /// disappeared or changed (rows / flags), in new ids.
    pub import_edges: Vec<EdgeDelta>,
    /// Call edges aggregated per (caller, callee) pair that appeared, disappeared or changed
    /// (site count / best confidence), in new ids.
    pub call_edges: Vec<EdgeDelta>,
    /// Override edges (base method -> overriding method) that appeared, disappeared or
    /// changed, in new ids.
    pub override_edges: Vec<EdgeDelta>,
    /// The interner was compacted: `Sym`s taken from the index before the update are invalid.
    pub strings_compacted: bool,
    pub(super) removed_qids: Vec<u8>,
    pub(super) removed_qid_ends: Vec<u32>,
}

impl IndexDiff {
    /// Qualified id of removed node `removed[i]`, as it was before the update.
    pub fn removed_qid(&self, i: usize) -> &[u8] {
        let start = if i == 0 { 0 } else { self.removed_qid_ends[i - 1] as usize };
        &self.removed_qids[start..self.removed_qid_ends[i] as usize]
    }

    /// Nothing changed.
    pub fn is_empty(&self) -> bool {
        self.added.is_empty()
            && self.removed.is_empty()
            && self.changed.is_empty()
            && self.moved.is_empty()
            && self.import_edges.is_empty()
            && self.call_edges.is_empty()
            && self.override_edges.is_empty()
            && self.added_files.is_empty()
            && self.removed_files.is_empty()
            && self.reextracted.is_empty()
    }

    /// Heap bytes (capacity).
    pub fn heap_bytes(&self) -> usize {
        (self.node_map.capacity()
            + self.added.capacity()
            + self.removed.capacity()
            + self.changed.capacity()
            + self.moved.capacity()
            + self.reextracted.capacity()
            + self.added_files.capacity()
            + self.removed_files.capacity()
            + self.removed_qid_ends.capacity())
            * 4
            + (self.import_edges.capacity() + self.call_edges.capacity() + self.override_edges.capacity()) * size_of::<EdgeDelta>()
            + self.removed_qids.capacity()
    }
}

/// An edge between two nodes of the new index, before and after the update (`None`: absent).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EdgeDelta {
    pub src: NodeId,
    pub dst: NodeId,
    pub old: Option<EdgeLabel>,
    pub new: Option<EdgeLabel>,
}

/// Weight of an aggregated edge: import rows and [`crate::resolve::res_flags`] for import
/// edges; call sites and the best [`crate::resolve::Confidence`] (as `u8`) for call edges.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EdgeLabel {
    pub count: u32,
    pub label: u8,
}

/// One def (or the `<module>` row) of an old file that is re-extracted or removed.
#[derive(Clone, Copy)]
struct OldDef {
    parent: NodeId,
    name: Option<Sym>,
    dup: u16,
    /// [`signature`]: content and position.
    sig: (u64, u64),
}

/// What the diff needs from the old index (see the module docs).
pub(super) struct OldSnap {
    nodes: u32,
    file_node: Vec<NodeId>,
    /// Defs of old files that are not kept, per such file: (file, first entry in `defs`).
    dropped: Vec<(FileId, u32)>,
    defs: Vec<OldDef>,
    /// Qualified ids of the nodes of dropped files and of nodes without a file.
    qids: Vec<u8>,
    qid_of: FxHashMap<NodeId, (u32, u32)>,
    /// Old nodes without a file (distributions, namespace packages).
    fileless: Vec<NodeId>,
    /// Duplicate ordinal and parent of every old node (only when file metadata changed).
    structure: Option<(Vec<u16>, Vec<u32>)>,
    /// Run starts per old file (`files + 1` entries) of the import, call and decorator tables:
    /// translate rows of carried resolution results.
    pub imports: Vec<u32>,
    pub calls: Vec<u32>,
    pub decorators: Vec<u32>,
}

/// Signature of def `v`: a hash of what it is (kind, flags, decorators, bases, params,
/// metrics) and a hash of where it is (line and byte span, name position).
fn signature(ix: &Index, v: NodeId) -> (u64, u64) {
    let (mut h, mut at) = (FxHasher::default(), FxHasher::default());
    let Some(row) = ix.def_row(v) else { return (0, 0) };
    let d = &ix.facts().defs;
    h.write_u8(d.kind[row]);
    h.write_u16(d.flags[row]);
    for x in [d.start_line[row], d.end_line[row], d.name_line[row], d.name_col[row], d.start[row], d.end[row]] {
        at.write_u32(x);
    }
    h.write_u8(d.decorators_len[row]);
    h.write_u8(d.bases_len[row]);
    h.write_u16(d.params_len[row]);
    if let Some(m) = ix.metrics_row(v) {
        let t = ix.metrics();
        h.write_u16(t.cyclomatic[m]);
        h.write_u16(t.cognitive[m]);
        h.write_u8(t.max_nesting[m]);
        h.write_u8(t.distinct_operators[m]);
        h.write_u8(t.params[m]);
        for x in [t.loc[m], t.distinct_operands[m], t.operators[m], t.operands[m], t.doc_lines[m]] {
            h.write_u32(x);
        }
    }
    (h.finish(), at.finish())
}

impl OldSnap {
    /// Snapshot of `ix` for an update with `src` (sources of the new files). `structure`: also
    /// keep duplicate ordinals and parents.
    pub fn take(ix: &Index, src: &[Src], structure: bool) -> OldSnap {
        let n = ix.file_count();
        let mut kept = vec![false; n as usize];
        for s in src {
            if let Src::Old(f) = *s {
                kept[f as usize] = true;
            }
        }
        let file_node: Vec<NodeId> = (0..n).map(|f| ix.file_node(f)).collect();
        let mut snap = OldSnap {
            nodes: ix.node_count(),
            file_node,
            dropped: Vec::new(),
            defs: Vec::new(),
            qids: Vec::new(),
            qid_of: FxHashMap::default(),
            fileless: Vec::new(),
            structure: None,
            imports: run_starts(ix, |ff| (ff.imports.start, ff.imports.len), ix.facts().imports.len()),
            calls: run_starts(ix, |ff| (ff.calls.start, ff.calls.len), ix.facts().calls.len()),
            decorators: run_starts(ix, |ff| (ff.decorators.start, ff.decorators.len), ix.facts().decorators.len()),
        };
        let push_qid = |snap: &mut OldSnap, v: NodeId| {
            let start = snap.qids.len() as u32;
            ix.qid(v, &mut snap.qids);
            snap.qid_of.insert(v, (start, snap.qids.len() as u32));
        };
        for f in 0..n {
            if kept[f as usize] {
                continue;
            }
            snap.dropped.push((f, snap.defs.len() as u32));
            let fnode = ix.file_node(f);
            for d in 0..ix.file_facts(f).defs.len {
                let v = fnode + d;
                snap.defs.push(OldDef {
                    parent: ix.parent(v).unwrap_or(NONE),
                    name: if d == 0 { None } else { Some(ix.name_sym(v)) },
                    dup: ix.dup(v),
                    sig: signature(ix, v),
                });
                push_qid(&mut snap, v);
            }
        }
        // Nodes without a file: distributions, namespace packages.
        let mut v = 0;
        while v < ix.node_count() {
            match ix.file(v) {
                None => {
                    snap.fileless.push(v);
                    push_qid(&mut snap, v);
                    v += 1;
                }
                // Skip the file's defs: they all have a file.
                Some(f) if ix.file_node(f) == v => v += ix.file_facts(f).defs.len,
                Some(_) => v += 1,
            }
        }
        if structure {
            let nn = ix.node_count();
            let dup = (0..nn).map(|v| ix.dup(v)).collect();
            let parent = (0..nn).map(|v| ix.parent(v).unwrap_or(NONE)).collect();
            snap.structure = Some((dup, parent));
        }
        snap
    }

    /// Names of the defs of the old files that are re-extracted or removed.
    pub fn dropped_names(&self) -> impl Iterator<Item = Sym> + '_ {
        self.defs.iter().filter_map(|d| d.name)
    }

    /// Whether a node of a re-extracted or removed file has no new node in `map`.
    pub fn any_dropped_gone(&self, map: &[NodeId]) -> bool {
        self.dropped.iter().enumerate().any(|(k, &(f, first))| {
            let end = self.dropped.get(k + 1).map_or(self.defs.len(), |&(_, s)| s as usize);
            let v0 = self.file_node[f as usize];
            (0..(end - first as usize) as u32).any(|d| map[(v0 + d) as usize] == NONE)
        })
    }

    /// Package/module node of every old file.
    pub fn file_nodes(&self) -> &[NodeId] {
        &self.file_node
    }

    /// Matches old nodes to the nodes of `ix` (the patched index) and fills the node parts of
    /// a diff. `old_of`: old file of each new file.
    pub fn diff_nodes(&self, ix: &Index, src: &[Src], old_of: &[u32]) -> IndexDiff {
        let mut map = vec![NONE; self.nodes as usize];
        let mut changed = Vec::new();
        let mut moved = Vec::new();
        let mut reextracted = Vec::new();
        let mut added_files = Vec::new();
        let mut keys: FxHashMap<(NodeId, Option<Sym>, u16), NodeId> = FxHashMap::default();
        let dropped_at: FxHashMap<FileId, usize> = self.dropped.iter().enumerate().map(|(k, &(f, _))| (f, k)).collect();
        for (i, s) in src.iter().enumerate() {
            let i = i as FileId;
            let nf = ix.file_node(i);
            match *s {
                Src::Old(f) => {
                    let of = self.file_node[f as usize];
                    for d in 0..ix.file_facts(i).defs.len {
                        map[(of + d) as usize] = nf + d;
                    }
                }
                Src::Fresh(_) => {
                    reextracted.push(i);
                    changed.push(nf);
                    let f = old_of[i as usize];
                    if f == u32::MAX {
                        added_files.push(i);
                        continue;
                    }
                    let of = self.file_node[f as usize];
                    map[of as usize] = nf;
                    let n_new = ix.file_facts(i).defs.len;
                    keys.clear();
                    for d in 1..n_new {
                        let v = nf + d;
                        keys.insert((ix.parent(v).unwrap_or(NONE), Some(ix.name_sym(v)), ix.dup(v)), v);
                    }
                    let k = dropped_at[&f];
                    let first = self.dropped[k].1 as usize;
                    let end = self.dropped.get(k + 1).map_or(self.defs.len(), |&(_, s)| s as usize);
                    for d in 1..end - first {
                        let od = self.defs[first + d];
                        let p = if od.parent == NONE { NONE } else { map[od.parent as usize] };
                        if p == NONE {
                            continue;
                        }
                        if let Some(&v) = keys.get(&(p, od.name, od.dup)) {
                            map[(of + d as u32) as usize] = v;
                            let sig = signature(ix, v);
                            if sig.0 != od.sig.0 {
                                changed.push(v);
                            } else if sig.1 != od.sig.1 {
                                moved.push(v);
                            }
                        }
                    }
                }
            }
        }
        let mut hit = vec![false; ix.node_count() as usize];
        for &w in &map {
            if w != NONE {
                hit[w as usize] = true;
            }
        }
        // Nodes without a file, and nodes of removed files: by qualified id, to a node no file
        // claimed (a package whose `__init__` was removed lives on as a namespace package).
        let removed_files = old_files_gone(&self.file_node, src, old_of);
        let mut by_qid = |v: NodeId, had_file: bool| {
            let (s, e) = self.qid_of[&v];
            if let Some(w) = ix.find(&self.qids[s as usize..e as usize])
                && !hit[w as usize]
            {
                map[v as usize] = w;
                hit[w as usize] = true;
                if had_file || ix.file(w).is_some() {
                    changed.push(w);
                }
            }
        };
        for &v in &self.fileless {
            by_qid(v, false);
        }
        for &f in &removed_files {
            let k = dropped_at[&f];
            let first = self.dropped[k].1 as usize;
            let end = self.dropped.get(k + 1).map_or(self.defs.len(), |&(_, s)| s as usize);
            for d in 0..(end - first) as u32 {
                by_qid(self.file_node[f as usize] + d, true);
            }
        }
        // Structure changes of matched nodes (only possible when file metadata changed).
        if let Some((dup, parent)) = &self.structure {
            for (v, &w) in map.iter().enumerate() {
                if w == NONE {
                    continue;
                }
                let p = parent[v];
                let mp = if p == NONE { NONE } else { map[p as usize] };
                if dup[v] != ix.dup(w) || mp != ix.parent(w).unwrap_or(NONE) {
                    changed.push(w);
                }
            }
        }
        let mut removed = Vec::new();
        let (mut removed_qids, mut removed_qid_ends) = (Vec::new(), Vec::new());
        for (v, &w) in map.iter().enumerate() {
            if w == NONE {
                removed.push(v as NodeId);
                if let Some(&(s, e)) = self.qid_of.get(&(v as NodeId)) {
                    removed_qids.extend_from_slice(&self.qids[s as usize..e as usize]);
                }
                removed_qid_ends.push(removed_qids.len() as u32);
            }
        }
        let added = (0..ix.node_count()).filter(|&w| !hit[w as usize]).collect();
        changed.sort_unstable();
        changed.dedup();
        moved.sort_unstable();
        IndexDiff {
            node_map: map,
            added,
            removed,
            changed,
            moved,
            reextracted,
            added_files,
            removed_files,
            removed_qids,
            removed_qid_ends,
            ..IndexDiff::default()
        }
    }
}

/// Start of every file's run of one table, plus the table length (`files + 1` entries).
fn run_starts(ix: &Index, run: impl Fn(&crate::extract::facts::FileFacts) -> (u32, u32), len: u32) -> Vec<u32> {
    let n = ix.file_count();
    let mut v = Vec::with_capacity(n as usize + 1);
    for f in 0..n {
        v.push(run(ix.file_facts(f)).0);
    }
    v.push(len);
    v
}

/// Old files that no new file comes from.
fn old_files_gone(file_node: &[NodeId], src: &[Src], old_of: &[u32]) -> Vec<FileId> {
    let mut alive = vec![false; file_node.len()];
    for (i, s) in src.iter().enumerate() {
        match *s {
            Src::Old(f) => alive[f as usize] = true,
            Src::Fresh(_) if old_of[i] != u32::MAX => alive[old_of[i] as usize] = true,
            Src::Fresh(_) => {}
        }
    }
    (0..file_node.len() as FileId).filter(|&f| !alive[f as usize]).collect()
}
