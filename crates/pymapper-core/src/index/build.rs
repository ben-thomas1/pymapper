//! Hierarchy construction: pre-order nodes, duplicate ordinals, subtree ends, ancestor paths
//! and the module lookup table.

use std::cmp::Ordering;

use super::inputs::is_init;
use super::{Files, Index, Inputs, LoadStats, NONE, NodeId, NodeKind, Nodes, ReadError};
use crate::extract::MetricsTable;
use crate::extract::facts::{DefKind, FactStore, FileFacts, def_flags};
use crate::intern::{FxHashMap, Interner, Sym};
use crate::roots::FileStatus;

/// Name of a def the parser could not name (recovered syntax error).
pub(super) const ANONYMOUS: &[u8] = b"<anonymous>";

/// Compares dotted names segment-wise (`a` < `a.b` < `a-b` < `a0`): `.` sorts before every
/// other byte, so a package is directly followed by everything below it.
pub(crate) fn cmp_dotted(a: &[u8], b: &[u8]) -> Ordering {
    let key = |c: u8| if c == b'.' { 0u16 } else { u16::from(c) + 1 };
    for (&x, &y) in a.iter().zip(b) {
        match key(x).cmp(&key(y)) {
            Ordering::Equal => {}
            o => return o,
        }
    }
    a.len().cmp(&b.len())
}

/// Order of files sharing a dotted name: the implementation first, then stubs, shadowed,
/// non-importable and outside files.
fn file_rank(status: FileStatus, path: &[u8]) -> u8 {
    match status {
        FileStatus::Active if path.ends_with(b".pyi") => 1,
        FileStatus::Active | FileStatus::StubShadowed => 0,
        FileStatus::Shadowed => 2,
        FileStatus::NotImportable => 3,
        FileStatus::Outside => 4,
    }
}

/// An open package/module of the current group: its node and its last dotted segment.
#[derive(Clone, Copy)]
struct Open {
    node: NodeId,
    seg: Sym,
}

struct Builder<'a> {
    strings: &'a mut Interner,
    facts: &'a FactStore,
    files: &'a [FileFacts],
    paths: &'a crate::walk::PathList,
    nodes: Nodes,
    file_node: Vec<NodeId>,
    /// Next duplicate ordinal per dotted package/module name.
    module_dups: FxHashMap<Sym, u16>,
    modules: FxHashMap<(Sym, u16), NodeId>,
    dups: DefDups,
    anonymous: Sym,
    /// Open packages/modules below the distribution node (`stack[0]` = depth 1).
    stack: Vec<Open>,
    /// Files waiting for their same-named primary's subtree to close:
    /// (stack level, file, last segment, dotted name).
    pending: Vec<(usize, u32, Sym, Sym)>,
    segs: Vec<Sym>,
    /// Reused copy of the current dotted name.
    name_buf: Vec<u8>,
}

#[allow(clippy::too_many_arguments)]
pub(super) fn build(
    inputs: Inputs,
    mut strings: Interner,
    facts: FactStore,
    metrics: MetricsTable,
    files: Vec<FileFacts>,
    read_errors: Vec<ReadError>,
    stats: LoadStats,
) -> Index {
    let n = files.len();
    let module: Vec<Sym> = (0..n).map(|f| strings.intern(inputs.module(f as u32))).collect();

    // Distributions by (origin, name); files by (distribution, dotted name, rank, FileId).
    let mut group_order: Vec<u32> = (0..inputs.groups.len() as u32).collect();
    group_order.sort_by(|&a, &b| {
        let (ga, gb) = (&inputs.groups[a as usize], &inputs.groups[b as usize]);
        (ga.origin, &ga.name, a).cmp(&(gb.origin, &gb.name, b))
    });
    let mut group_rank = vec![0u32; group_order.len()];
    for (r, &g) in group_order.iter().enumerate() {
        group_rank[g as usize] = r as u32;
    }
    let mut order: Vec<u32> = (0..n as u32).collect();
    order.sort_unstable_by(|&a, &b| {
        let (ia, ib) = (a as usize, b as usize);
        group_rank[inputs.group[ia] as usize]
            .cmp(&group_rank[inputs.group[ib] as usize])
            .then_with(|| cmp_dotted(strings.resolve(module[ia]), strings.resolve(module[ib])))
            .then_with(|| {
                file_rank(inputs.status[ia], inputs.paths.get_bytes(ia))
                    .cmp(&file_rank(inputs.status[ib], inputs.paths.get_bytes(ib)))
            })
            .then(a.cmp(&b))
    });

    let total_defs: usize = files.iter().map(|ff| ff.defs.len as usize).sum();
    let anonymous = strings.intern(ANONYMOUS);
    let mut b = Builder {
        strings: &mut strings,
        facts: &facts,
        files: &files,
        paths: &inputs.paths,
        nodes: Nodes::default(),
        file_node: vec![NONE; n],
        module_dups: FxHashMap::default(),
        modules: FxHashMap::default(),
        dups: DefDups::default(),
        anonymous,
        stack: Vec::new(),
        pending: Vec::new(),
        segs: Vec::new(),
        name_buf: Vec::new(),
    };
    // Namespace packages are extra; most trees have few.
    b.nodes.reserve(group_order.len() + total_defs + total_defs / 64);

    let mut dist_dups: FxHashMap<Sym, u16> = FxHashMap::default();
    let mut group_node = vec![NONE; group_order.len()];
    let mut next = 0usize;
    for &g in &group_order {
        let name = b.strings.intern(&inputs.groups[g as usize].name);
        let d = dist_dups.entry(name).or_insert(0);
        let dist = b.nodes.push(NodeKind::Distribution, *d, 0, NONE, name, NONE);
        *d += 1;
        group_node[g as usize] = dist;
        while next < n && inputs.group[order[next] as usize] == g {
            let f = order[next];
            b.file(dist, f, module[f as usize]);
            next += 1;
        }
        b.close_to(dist, 0);
    }
    let Builder { nodes, file_node, modules, .. } = b;
    let mut nodes = nodes;
    let chain = finish(&mut nodes);

    let Inputs { paths, groups, group, status, .. } = inputs;
    Index {
        strings,
        paths,
        groups,
        files: Files { facts: files, group, status, module, node: file_node },
        facts,
        metrics,
        nodes,
        chain,
        group_node,
        // Distribution nodes are pushed in `group_order`, so node ids ascend along it.
        groups_by_node: group_order,
        modules,
        read_errors,
        stats,
    }
}

impl Builder<'_> {
    /// Places file `f` (dotted name `module`) in the tree of distribution `dist`.
    fn file(&mut self, dist: NodeId, f: u32, module: Sym) {
        let mut name = std::mem::take(&mut self.name_buf);
        name.clear();
        name.extend_from_slice(self.strings.resolve(module));
        self.place(dist, f, module, &name);
        self.name_buf = name;
    }

    /// [`Builder::file`] with the bytes of the dotted name in `name`.
    fn place(&mut self, dist: NodeId, f: u32, module: Sym, name: &[u8]) {
        self.segs.clear();
        for seg in name.split(|&c| c == b'.') {
            let s = self.strings.intern(seg);
            self.segs.push(s);
        }
        let mut k = 0;
        while k < self.stack.len() && k < self.segs.len() && self.stack[k].seg == self.segs[k] {
            k += 1;
        }
        if k == self.segs.len() {
            let seg = self.segs[k - 1];
            if self.nodes.file[self.stack[k - 1].node as usize] != NONE {
                // Same name as an open file node: emitted once its subtree is closed.
                self.close_to(dist, k);
                self.pending.push((k - 1, f, seg, module));
            } else {
                // Cannot happen for sorted input (a file sorts before the names below it);
                // keep the node a sibling of the namespace package.
                self.close_to(dist, k - 1);
                let parent = self.top(dist);
                self.emit_file(parent, f, seg, module);
            }
            return;
        }
        self.close_to(dist, k);
        // Namespace packages for missing intermediate segments.
        let mut off: usize = self.segs[..k].iter().map(|&s| self.strings.resolve(s).len() + 1).sum();
        for j in k..self.segs.len() - 1 {
            let seg = self.segs[j];
            off += self.strings.resolve(seg).len();
            let dotted = self.strings.intern(&name[..off]);
            off += 1;
            let parent = self.top(dist);
            let v = self.push_module(NodeKind::Package, parent, seg, dotted, NONE);
            self.stack.push(Open { node: v, seg });
        }
        let parent = self.top(dist);
        let seg = *self.segs.last().expect("dotted names are non-empty");
        let v = self.emit_file(parent, f, seg, module);
        self.stack.push(Open { node: v, seg });
    }

    #[inline]
    fn top(&self, dist: NodeId) -> NodeId {
        self.stack.last().map_or(dist, |o| o.node)
    }

    /// Closes open entries until `level` remain, emitting their pending duplicates after
    /// each closed subtree.
    fn close_to(&mut self, dist: NodeId, level: usize) {
        while self.stack.len() > level {
            let lvl = self.stack.len() - 1;
            self.stack.pop();
            // This level's duplicates are the tail of `pending`; emit them in rank order.
            let mut start = self.pending.len();
            while start > 0 && self.pending[start - 1].0 == lvl {
                start -= 1;
            }
            let parent = self.top(dist);
            for i in start..self.pending.len() {
                let (_, f, seg, module) = self.pending[i];
                self.emit_file(parent, f, seg, module);
            }
            self.pending.truncate(start);
        }
    }

    /// Pushes a package/module node with the next duplicate ordinal of its dotted name.
    fn push_module(&mut self, kind: NodeKind, parent: NodeId, seg: Sym, dotted: Sym, file: u32) -> NodeId {
        let d = self.module_dups.entry(dotted).or_insert(0);
        let dup = *d;
        *d = d.saturating_add(1);
        let depth = self.nodes.depth[parent as usize] + 1;
        let v = self.nodes.push(kind, dup, depth, parent, seg, file);
        self.modules.insert((dotted, dup), v);
        v
    }

    /// Emits the node of file `f` followed by its defs (pre-order, source order).
    fn emit_file(&mut self, parent: NodeId, f: u32, seg: Sym, module: Sym) -> NodeId {
        let kind = if is_init(self.paths.get_bytes(f as usize)) { NodeKind::Package } else { NodeKind::Module };
        let m = self.push_module(kind, parent, seg, module, f);
        self.file_node[f as usize] = m;
        push_defs(&mut self.nodes, self.facts, &self.files[f as usize], m, f, self.anonymous, &mut self.dups);
        m
    }
}

/// Scratch of [`push_defs`].
#[derive(Default)]
pub(super) struct DefDups {
    /// Duplicate ordinals of defs of the current file, by (parent, name).
    def_dups: FxHashMap<(u32, Sym), u16>,
    /// Files with `@overload`s: the implementation row (last non-`@overload` def) of each
    /// (parent, name) group that has overloads (`NONE`: none yet).
    impl_rows: FxHashMap<(u32, Sym), (u32, bool)>,
}

/// Pushes the def nodes of file `f` (header `ff`), whose package/module node `m` was just
/// pushed: pre-order, source order, with duplicate ordinals (see the module docs of `index`).
pub(super) fn push_defs(nodes: &mut Nodes, facts: &FactStore, ff: &FileFacts, m: NodeId, f: u32, anonymous: Sym, dd: &mut DefDups) {
    let defs = &facts.defs;
    let r = ff.defs.range();
    let dedupe = r.len() > 2;
    if dedupe {
        dd.def_dups.clear();
    }
    // A group of same-named siblings with `@overload` signatures: the implementation (the
    // last non-overload def) is the unsuffixed id, the signatures get `#2`, `#3`, ... in
    // source order. Only files with overloads pay for the extra pass.
    dd.impl_rows.clear();
    let overloads = dedupe && (r.start + 1..r.end).any(|row| defs.flags[row] & def_flags::OVERLOAD != 0);
    if overloads {
        for row in r.start + 1..r.end {
            let key = (m + defs.parent[row], defs.name[row].unwrap_or(anonymous));
            let e = dd.impl_rows.entry(key).or_insert((NONE, false));
            if defs.flags[row] & def_flags::OVERLOAD != 0 {
                e.1 = true;
            } else {
                e.0 = row as u32;
            }
        }
    }
    for row in r.start + 1..r.end {
        let parent = m + defs.parent[row];
        let kind = if defs.kind[row] == DefKind::Class as u8 { NodeKind::Class } else { NodeKind::Function };
        let name = defs.name[row].unwrap_or(anonymous);
        let dup = if dedupe {
            let imp = match dd.impl_rows.get(&(parent, name)) {
                Some(&(i, true)) if i != NONE => Some(i),
                _ => None,
            };
            if imp == Some(row as u32) {
                0
            } else {
                let d = dd.def_dups.entry((parent, name)).or_insert(u16::from(imp.is_some()));
                let dup = *d;
                *d = d.saturating_add(1);
                dup
            }
        } else {
            0
        };
        let depth = nodes.depth[parent as usize] + 1;
        nodes.push(kind, dup, depth, parent, name, f);
    }
}

/// Fills subtree ends (reverse pass) and the ancestor paths of inner nodes (forward pass);
/// returns the ancestor pool.
pub(super) fn finish(nodes: &mut Nodes) -> Vec<u32> {
    let n = nodes.len() as usize;
    for v in (0..n).rev() {
        let p = nodes.parent[v];
        if p != NONE {
            let e = nodes.end[v];
            if e > nodes.end[p as usize] {
                nodes.end[p as usize] = e;
            }
        }
    }
    let mut anc = vec![NONE; n];
    let mut chain = Vec::new();
    for v in 0..n {
        if nodes.end[v] as usize == v + 1 {
            continue;
        }
        let at = chain.len() as u32;
        let p = nodes.parent[v];
        if p != NONE {
            let s = anc[p as usize] as usize;
            let len = usize::from(nodes.depth[p as usize]) + 1;
            chain.extend_from_within(s..s + len);
        }
        chain.push(v as u32);
        anc[v] = at;
    }
    nodes.anc = anc;
    chain.shrink_to_fit();
    chain
}
