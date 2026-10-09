//! Edge deltas of an update: module import edges and call edges per (caller, callee) pair.
//!
//! Edges are compared file by file (edges of a file are stored together in both resolutions).
//! A file whose old edges, translated to new nodes, equal its new edges element by element is
//! skipped with one linear compare; only files that differ are aggregated (sorted by pair) and
//! merged. Old edges with an endpoint that is gone are dropped (they vanish with the node).

use super::diff::{EdgeDelta, EdgeLabel};
use crate::index::{FileId, Index, NONE, NodeId};
use crate::resolve::{CallEdge, Confidence, Imports, Reason, ResolvedCalls};

#[inline]
fn tr(map: &[NodeId], v: NodeId) -> NodeId {
    map.get(v as usize).copied().unwrap_or(NONE)
}

/// Appends the deltas of `old` (sorted by pair, old labels) against `new` (sorted, new labels).
fn merge(src: NodeId, old: &[(NodeId, EdgeLabel)], new: &[(NodeId, EdgeLabel)], out: &mut Vec<EdgeDelta>) {
    let (mut i, mut j) = (0, 0);
    while i < old.len() || j < new.len() {
        let o = old.get(i);
        let n = new.get(j);
        match (o, n) {
            (Some(&(od, ol)), Some(&(nd, nl))) if od == nd => {
                if ol != nl {
                    out.push(EdgeDelta { src, dst: od, old: Some(ol), new: Some(nl) });
                }
                i += 1;
                j += 1;
            }
            (Some(&(od, ol)), Some(&(nd, _))) if od < nd => {
                out.push(EdgeDelta { src, dst: od, old: Some(ol), new: None });
                i += 1;
            }
            (Some(&(od, ol)), None) => {
                out.push(EdgeDelta { src, dst: od, old: Some(ol), new: None });
                i += 1;
            }
            (_, Some(&(nd, nl))) => {
                out.push(EdgeDelta { src, dst: nd, old: None, new: Some(nl) });
                j += 1;
            }
            (None, None) => break,
        }
    }
}

/// Import edge deltas of every new file. `old_of`: old file of each new file (`u32::MAX`:
/// added); `removed`: old files that are gone (their edges count for the new file their module
/// node maps to, see [`super::diff`]; `old_fnode`: old file nodes). `skip(f)`: the file's
/// edges are known to be unchanged.
#[allow(clippy::too_many_arguments)]
pub(super) fn import_deltas(
    ix: &Index,
    map: &[NodeId],
    old: &Imports,
    new: &Imports,
    old_of: &[u32],
    removed: &[FileId],
    old_fnode: &[NodeId],
    skip: impl Fn(FileId) -> bool,
    out: &mut Vec<EdgeDelta>,
) {
    // Edges of removed files whose module node lives on: (new file, dst, label).
    let mut orphans: Vec<(FileId, NodeId, EdgeLabel)> = Vec::new();
    let mut gone: Vec<(NodeId, EdgeLabel)> = Vec::new();
    for &f in removed {
        let s = tr(map, old_fnode[f as usize]);
        if s == NONE {
            continue;
        }
        gone.clear();
        for e in old.module_edges(f) {
            let d = tr(map, e.dst);
            if d != NONE {
                gone.push((d, EdgeLabel { count: e.rows, label: e.flags }));
            }
        }
        match ix.file(s) {
            Some(i) => orphans.extend(gone.iter().map(|&(d, l)| (i, d, l))),
            None => {
                gone.sort_unstable_by_key(|x| x.0);
                merge(s, &gone, &[], out);
            }
        }
    }
    orphans.sort_unstable_by_key(|x| x.0);
    let mut o: Vec<(NodeId, EdgeLabel)> = Vec::new();
    let mut n: Vec<(NodeId, EdgeLabel)> = Vec::new();
    for i in 0..ix.file_count() {
        let orph = {
            let a = orphans.partition_point(|x| x.0 < i);
            let b = orphans.partition_point(|x| x.0 <= i);
            &orphans[a..b]
        };
        if skip(i) && orph.is_empty() {
            continue;
        }
        let ne = new.module_edges(i);
        let f = old_of[i as usize];
        let oe = if f == u32::MAX { &[][..] } else { old.module_edges(f) };
        if orph.is_empty()
            && oe.len() == ne.len()
            && oe.iter().zip(ne).all(|(a, b)| tr(map, a.dst) == b.dst && a.rows == b.rows && a.flags == b.flags)
        {
            continue;
        }
        o.clear();
        for e in oe {
            let d = tr(map, e.dst);
            if d != NONE {
                o.push((d, EdgeLabel { count: e.rows, label: e.flags }));
            }
        }
        o.extend(orph.iter().map(|&(_, d, l)| (d, l)));
        o.sort_unstable_by_key(|x| x.0);
        n.clear();
        n.extend(ne.iter().map(|e| (e.dst, EdgeLabel { count: e.rows, label: e.flags })));
        merge(ix.file_node(i), &o, &n, out);
    }
}

/// Per-pair aggregation buffer: (src, dst, sites, best confidence).
type Pairs = Vec<(NodeId, NodeId, u32, u8)>;

/// Sorts and aggregates `v` per (src, dst) pair in place.
fn aggregate(v: &mut Pairs) {
    v.sort_unstable_by_key(|x| (x.0, x.1));
    let mut w = 0;
    for r in 0..v.len() {
        let x = v[r];
        if w > 0 && v[w - 1].0 == x.0 && v[w - 1].1 == x.1 {
            v[w - 1].2 += x.2;
            v[w - 1].3 = v[w - 1].3.min(x.3);
        } else {
            v[w] = x;
            w += 1;
        }
    }
    v.truncate(w);
}

/// Appends the deltas of two aggregated pair lists.
fn merge_pairs(old: &Pairs, new: &Pairs, out: &mut Vec<EdgeDelta>) {
    let label = |x: &(NodeId, NodeId, u32, u8)| EdgeLabel { count: x.2, label: x.3 };
    let (mut i, mut j) = (0, 0);
    while i < old.len() || j < new.len() {
        let ko = old.get(i).map(|x| (x.0, x.1));
        let kn = new.get(j).map(|x| (x.0, x.1));
        match (ko, kn) {
            (Some(a), Some(b)) if a == b => {
                let (lo, ln) = (label(&old[i]), label(&new[j]));
                if lo != ln {
                    out.push(EdgeDelta { src: a.0, dst: a.1, old: Some(lo), new: Some(ln) });
                }
                i += 1;
                j += 1;
            }
            (Some(a), Some(b)) if a < b => {
                out.push(EdgeDelta { src: a.0, dst: a.1, old: Some(label(&old[i])), new: None });
                i += 1;
            }
            (Some(a), None) => {
                out.push(EdgeDelta { src: a.0, dst: a.1, old: Some(label(&old[i])), new: None });
                i += 1;
            }
            (_, Some(b)) => {
                out.push(EdgeDelta { src: b.0, dst: b.1, old: None, new: Some(label(&new[j])) });
                j += 1;
            }
            (None, None) => break,
        }
    }
}

#[inline]
fn same_edge(map: &[NodeId], a: &CallEdge, b: &CallEdge) -> bool {
    tr(map, a.src) == b.src && tr(map, a.dst) == b.dst && a.conf == b.conf && a.reason == b.reason
}

/// Call edge deltas of every new file: call and decorator edges into `calls`, override edges
/// (base method -> override, listed with the overriding class's file) into `overrides`.
#[allow(clippy::too_many_arguments)]
pub(super) fn call_deltas(
    ix: &Index,
    map: &[NodeId],
    old: &ResolvedCalls,
    new: &ResolvedCalls,
    old_of: &[u32],
    removed: &[FileId],
    skip: impl Fn(FileId) -> bool,
    calls: &mut Vec<EdgeDelta>,
    overrides: &mut Vec<EdgeDelta>,
) {
    let counted = |e: &CallEdge| e.dst != NONE && e.conf != Confidence::Unresolved;
    // Edges of removed files with both ends alive: (new file listing them, src, dst, conf,
    // override). Edges listed by no file (their owner is a namespace package) are gone.
    let mut orphans: Vec<(FileId, NodeId, NodeId, u8, bool)> = Vec::new();
    let mut gone: [Pairs; 2] = Default::default();
    for &f in removed {
        for e in old.file_edges(f) {
            let (s, d) = (tr(map, e.src), tr(map, e.dst));
            if s == NONE || d == NONE || !counted(e) {
                continue;
            }
            let ov = e.reason == Reason::Override;
            match ix.file(if ov { d } else { s }) {
                Some(i) => orphans.push((i, s, d, e.conf as u8, ov)),
                None => gone[usize::from(ov)].push((s, d, 1, e.conf as u8)),
            }
        }
    }
    let [gc, go] = &mut gone;
    aggregate(gc);
    aggregate(go);
    merge_pairs(gc, &Vec::new(), calls);
    merge_pairs(go, &Vec::new(), overrides);
    orphans.sort_unstable_by_key(|x| x.0);
    let mut bufs: [Pairs; 4] = Default::default();
    for i in 0..ix.file_count() {
        let orph = {
            let a = orphans.partition_point(|x| x.0 < i);
            let b = orphans.partition_point(|x| x.0 <= i);
            &orphans[a..b]
        };
        if skip(i) && orph.is_empty() {
            continue;
        }
        let ne = new.file_edges(i);
        let f = old_of[i as usize];
        let oe = if f == u32::MAX { &[][..] } else { old.file_edges(f) };
        if orph.is_empty() && oe.len() == ne.len() && oe.iter().zip(ne).all(|(a, b)| same_edge(map, a, b)) {
            continue;
        }
        let [oc, oo, nc, no] = &mut bufs;
        for b in [&mut *oc, &mut *oo, &mut *nc, &mut *no] {
            b.clear();
        }
        let push = |e: &CallEdge, src: NodeId, dst: NodeId, c: &mut Pairs, o: &mut Pairs| {
            if !counted(e) {
                return;
            }
            let row = (src, dst, 1, e.conf as u8);
            if e.reason == Reason::Override { o.push(row) } else { c.push(row) }
        };
        for e in oe {
            let (s, d) = (tr(map, e.src), tr(map, e.dst));
            if s != NONE && d != NONE {
                push(e, s, d, oc, oo);
            }
        }
        for &(_, s, d, c, ov) in orph {
            if ov { oo.push((s, d, 1, c)) } else { oc.push((s, d, 1, c)) }
        }
        for e in ne {
            push(e, e.src, e.dst, nc, no);
        }
        for b in [&mut *oc, &mut *oo, &mut *nc, &mut *no] {
            aggregate(b);
        }
        merge_pairs(oc, nc, calls);
        merge_pairs(oo, no, overrides);
    }
}
