//! The class table of the call resolver: every class of the index with its members (sorted by
//! name for binary search) and its C3 method resolution order.

use rayon::prelude::*;

use crate::extract::facts::{DefKind, ValueKind, def_flags};
use crate::index::{FileId, Index, NONE, NodeId};
use crate::intern::Sym;

/// Index of a class in a [`Classes`] table (FileId order, then source order).
pub type ClassId = u32;

/// What a class member is ([`Member::kind`]).
pub(super) mod mkind {
    /// A method or nested class defined in the body: `r` is its node.
    pub const DEF: u8 = 0;
    /// An instance attribute assigned in a method (`self.x = Foo()`): `r` is the binding row.
    /// Only the first binding is kept, preferring construction values.
    pub const IATTR: u8 = 1;
    /// A class-body assignment (`x = ...`): `r` is the binding row (the first one).
    pub const VAR: u8 = 2;
}

/// One member of a class (12 bytes). At most one per (name, kind).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Member {
    pub name: Sym,
    pub r: u32,
    pub kind: u8,
}

/// Class flags ([`Classes::flags`]).
pub mod class_flags {
    /// A base did not resolve to a loaded class (other than `object` and typing markers such
    /// as `Generic[T]` / `Protocol` / `ABC`): attributes may come from code that is not loaded.
    pub const INCOMPLETE: u8 = 1 << 0;
    /// The bases admit no C3 linearisation (or form a cycle); the MRO is a depth-first
    /// left-to-right fallback.
    pub const NO_C3: u8 = 1 << 1;
    /// The class body or a method assigns `__getattr__` / defines it (attributes are dynamic).
    pub const GETATTR: u8 = 1 << 2;
}

/// Every class of an index: struct of arrays indexed by [`ClassId`].
#[derive(Debug, Default)]
pub struct Classes {
    /// Class node per id; increasing within each file's run.
    pub(super) node: Vec<NodeId>,
    /// First class id of every file (`files + 1` entries).
    pub(super) file_start: Vec<u32>,
    pub(super) mem_start: Vec<u32>,
    pub(super) members: Vec<Member>,
    /// Direct bases that resolved to loaded classes, per class (`base_start` has `n + 1`).
    pub(super) base_start: Vec<u32>,
    pub(super) bases: Vec<ClassId>,
    /// The MRO of class `c` (itself first) is `mro[mro_off[c]..mro_off[c] + mro_len[c]]`.
    pub(super) mro_off: Vec<u32>,
    pub(super) mro_len: Vec<u16>,
    pub(super) mro: Vec<ClassId>,
    pub(super) flags: Vec<u8>,
}

/// Longest MRO kept (deeper hierarchies are cut; real ones are far shorter).
const MAX_MRO: usize = 1024;

/// Per-task scratch of the member pass.
#[derive(Default)]
pub(super) struct MemberScratch {
    /// File-relative def row -> local class index (or `NONE`).
    local: Vec<u32>,
    /// (local class, sort key, member).
    rows: Vec<(u32, u32, u8, u32, Member)>,
}

/// Output of the member pass for a chunk of files.
#[derive(Default)]
pub(super) struct MemberChunk {
    node: Vec<NodeId>,
    per_file: Vec<u32>,
    mem_len: Vec<u32>,
    members: Vec<Member>,
    flags: Vec<u8>,
}

impl Classes {
    /// Builds the class list and members of every class (parallel per file). Bases and MROs
    /// are filled in later ([`Classes::set_bases`], [`Classes::linearize`]).
    pub(super) fn build(ix: &Index, self_sym: Option<Sym>, getattr: Option<Sym>) -> Classes {
        let files = ix.file_count() as usize;
        let chunks = super::calls::par_chunks(files, |sc: &mut MemberScratch, f, out: &mut MemberChunk| {
            file_members(ix, f, self_sym, getattr, sc, out);
        });
        let n: usize = chunks.iter().map(|c| c.node.len()).sum();
        let m: usize = chunks.iter().map(|c| c.members.len()).sum();
        let mut t = Classes {
            node: Vec::with_capacity(n),
            file_start: Vec::with_capacity(files + 1),
            mem_start: Vec::with_capacity(n + 1),
            members: Vec::with_capacity(m),
            flags: Vec::with_capacity(n),
            ..Classes::default()
        };
        t.file_start.push(0);
        t.mem_start.push(0);
        for mut c in chunks {
            for k in c.per_file {
                let at = *t.file_start.last().expect("non-empty") + k;
                t.file_start.push(at);
            }
            for k in c.mem_len {
                let at = *t.mem_start.last().expect("non-empty") + k;
                t.mem_start.push(at);
            }
            t.node.append(&mut c.node);
            t.members.append(&mut c.members);
            t.flags.append(&mut c.flags);
        }
        // Until linearised, every class is its own MRO.
        t.mro_off = (0..n as u32).collect();
        t.mro_len = vec![1; n];
        t.mro = (0..n as u32).collect();
        t
    }

    /// Number of classes.
    #[inline]
    pub fn len(&self) -> u32 {
        self.node.len() as u32
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.node.is_empty()
    }

    /// Node of class `c`.
    #[inline]
    pub fn node(&self, c: ClassId) -> NodeId {
        self.node[c as usize]
    }

    /// [`class_flags`] of `c`.
    #[inline]
    pub fn flags(&self, c: ClassId) -> u8 {
        self.flags[c as usize]
    }

    /// The class id of class node `v` (binary search over the classes of `v`'s file).
    #[inline]
    pub fn id(&self, ix: &Index, v: NodeId) -> Option<ClassId> {
        let f = ix.file(v)? as usize;
        let (lo, hi) = (self.file_start[f] as usize, self.file_start[f + 1] as usize);
        let run = &self.node[lo..hi];
        run.binary_search(&v).ok().map(|i| (lo + i) as ClassId)
    }

    /// Classes of file `f`.
    #[inline]
    pub fn file_classes(&self, f: FileId) -> std::ops::Range<ClassId> {
        self.file_start[f as usize]..self.file_start[f as usize + 1]
    }

    /// The method resolution order of `c` (itself first), as class ids.
    #[inline]
    pub fn mro(&self, c: ClassId) -> &[ClassId] {
        let off = self.mro_off[c as usize] as usize;
        &self.mro[off..off + self.mro_len[c as usize] as usize]
    }

    /// Direct bases of `c` that resolved to loaded classes, in source order.
    #[inline]
    pub fn bases(&self, c: ClassId) -> &[ClassId] {
        if self.base_start.is_empty() {
            return &[];
        }
        &self.bases[self.base_start[c as usize] as usize..self.base_start[c as usize + 1] as usize]
    }

    /// Members of `c` named `name` (at most one per [`mkind`], in kind order).
    #[inline]
    pub(super) fn members(&self, c: ClassId, name: Sym) -> &[Member] {
        let run = &self.members[self.mem_start[c as usize] as usize..self.mem_start[c as usize + 1] as usize];
        let i = run.partition_point(|m| m.name.to_raw() < name.to_raw());
        let j = i + run[i..].iter().take_while(|m| m.name == name).count();
        &run[i..j]
    }

    /// All members of `c`, sorted by (name, kind).
    #[inline]
    pub(super) fn all_members(&self, c: ClassId) -> &[Member] {
        &self.members[self.mem_start[c as usize] as usize..self.mem_start[c as usize + 1] as usize]
    }

    /// Records the direct bases (`per_class[c]` = resolved base ids, plus flags to OR in).
    pub(super) fn set_bases(&mut self, bases: Vec<ClassId>, base_start: Vec<u32>, flags: &[u8]) {
        debug_assert_eq!(base_start.len(), self.node.len() + 1);
        self.bases = bases;
        self.base_start = base_start;
        for (f, &add) in self.flags.iter_mut().zip(flags) {
            *f |= add;
        }
    }

    /// Computes the C3 linearisation of every class from the direct bases. Sequential
    /// depth-first over the base graph (each class once); a class on a base cycle loses the
    /// cyclic base and gets [`class_flags::NO_C3`].
    pub(super) fn linearize(&mut self) {
        let n = self.node.len();
        self.mro_off = vec![NONE; n];
        self.mro_len = vec![0; n];
        self.mro = Vec::with_capacity(n * 2);
        // 0 = new, 1 = on the stack, 2 = done.
        let mut state = vec![0u8; n];
        let mut stack: Vec<ClassId> = Vec::new();
        let mut lin = Linearizer::default();
        for root in 0..n as ClassId {
            if state[root as usize] == 2 {
                continue;
            }
            stack.push(root);
            while let Some(&c) = stack.last() {
                let ci = c as usize;
                if state[ci] == 0 {
                    state[ci] = 1;
                    for &b in self.bases(c).iter().rev() {
                        if state[b as usize] == 0 {
                            stack.push(b);
                        }
                    }
                    continue;
                }
                stack.pop();
                if state[ci] == 2 {
                    continue;
                }
                // Every base is done now, except bases on a cycle through `c`.
                let mut cyclic = false;
                lin.begin(c);
                let (bs, be) = (self.base_start[ci] as usize, self.base_start[ci + 1] as usize);
                for k in bs..be {
                    let b = self.bases[k];
                    if state[b as usize] != 2 {
                        cyclic = true;
                        continue;
                    }
                    let off = self.mro_off[b as usize] as usize;
                    lin.add(&self.mro[off..off + self.mro_len[b as usize] as usize], b);
                    // Bases are final: inherit what makes their attributes uncertain.
                    self.flags[ci] |= self.flags[b as usize] & (class_flags::INCOMPLETE | class_flags::GETATTR);
                }
                let ok = lin.merge();
                if cyclic || !ok {
                    self.flags[ci] |= class_flags::NO_C3;
                }
                let out = lin.result();
                let len = out.len().min(MAX_MRO);
                self.mro_off[ci] = self.mro.len() as u32;
                self.mro_len[ci] = len as u16;
                self.mro.extend_from_slice(&out[..len]);
                state[ci] = 2;
            }
        }
        self.mro.shrink_to_fit();
    }

    /// Whether `anc` is in the MRO of `c` (a short scan).
    #[inline]
    pub fn inherits(&self, c: ClassId, anc: ClassId) -> bool {
        self.mro(c).contains(&anc)
    }

    pub fn heap_bytes(&self) -> usize {
        (self.node.capacity()
            + self.file_start.capacity()
            + self.mem_start.capacity()
            + self.base_start.capacity()
            + self.bases.capacity()
            + self.mro_off.capacity()
            + self.mro.capacity())
            * 4
            + self.members.capacity() * size_of::<Member>()
            + self.mro_len.capacity() * 2
            + self.flags.capacity()
    }
}

/// C3 merge state, reused across classes.
#[derive(Default)]
struct Linearizer {
    /// Concatenated input sequences: base MROs, then the list of direct bases.
    buf: Vec<ClassId>,
    /// (start, end) of every sequence in `buf`; `start` advances as heads are taken.
    seqs: Vec<(u32, u32)>,
    bases: Vec<ClassId>,
    out: Vec<ClassId>,
}

impl Linearizer {
    fn begin(&mut self, c: ClassId) {
        self.buf.clear();
        self.seqs.clear();
        self.bases.clear();
        self.out.clear();
        self.out.push(c);
    }

    fn add(&mut self, mro: &[ClassId], base: ClassId) {
        let s = self.buf.len() as u32;
        self.buf.extend_from_slice(mro);
        self.seqs.push((s, self.buf.len() as u32));
        self.bases.push(base);
    }

    /// Runs the merge into `out`. On failure (no consistent order) falls back to appending
    /// the remaining classes depth-first, left to right, without duplicates; returns false.
    fn merge(&mut self) -> bool {
        let s = self.buf.len() as u32;
        self.buf.extend_from_slice(&self.bases);
        self.seqs.push((s, self.buf.len() as u32));
        let mut ok = true;
        loop {
            let mut pick = None;
            let mut any = false;
            for i in 0..self.seqs.len() {
                let (st, en) = self.seqs[i];
                if st == en {
                    continue;
                }
                any = true;
                let cand = self.buf[st as usize];
                let in_tail = self.seqs.iter().any(|&(s2, e2)| s2 < e2 && self.buf[s2 as usize + 1..e2 as usize].contains(&cand));
                if !in_tail {
                    pick = Some(cand);
                    break;
                }
            }
            if !any {
                break;
            }
            let Some(cand) = pick else {
                ok = false;
                break;
            };
            self.out.push(cand);
            if self.out.len() > MAX_MRO {
                break;
            }
            for (st, en) in self.seqs.iter_mut() {
                if *st < *en && self.buf[*st as usize] == cand {
                    *st += 1;
                }
            }
        }
        if !ok {
            for &(st, en) in &self.seqs {
                for &c in &self.buf[st as usize..en as usize] {
                    if !self.out.contains(&c) && self.out.len() <= MAX_MRO {
                        self.out.push(c);
                    }
                }
            }
        }
        ok
    }

    fn result(&self) -> &[ClassId] {
        &self.out
    }
}

/// Appends the classes of file `f` and their members to `out`.
fn file_members(ix: &Index, f: FileId, self_sym: Option<Sym>, getattr: Option<Sym>, sc: &mut MemberScratch, out: &mut MemberChunk) {
    let st = ix.facts();
    let ff = ix.file_facts(f);
    let fv = ix.file_view(f);
    let d0 = ff.defs.start as usize;
    let nd = ff.defs.len as usize;
    sc.local.clear();
    sc.local.resize(nd, NONE);
    sc.rows.clear();
    let first = out.node.len();
    for d in 1..nd {
        if st.defs.kind[d0 + d] == DefKind::Class as u8 {
            sc.local[d] = (out.node.len() - first) as u32;
            out.node.push(ix.def_node(f, d as u32));
            out.flags.push(0);
        }
    }
    let count = out.node.len() - first;
    out.per_file.push(count as u32);
    if count == 0 {
        return;
    }
    let class_of = |d: u32| -> u32 { if (d as usize) < nd { sc.local[d as usize] } else { NONE } };
    for d in 1..nd {
        let row = d0 + d;
        let p = st.defs.parent[row];
        let c = class_of(p);
        if c == NONE {
            continue;
        }
        let Some(name) = st.defs.name[row] else { continue };
        // Python keeps the last definition; overloads only describe the implementation.
        let overload = u32::from(st.defs.flags[row] & def_flags::OVERLOAD != 0);
        let key = (overload << 31) | ((u32::MAX >> 1) - d as u32);
        let m = Member { name, r: ix.def_node(f, d as u32), kind: mkind::DEF };
        sc.rows.push((c, name.to_raw(), mkind::DEF, key, m));
        if Some(name) == getattr && st.defs.kind[row] == DefKind::Function as u8 {
            let i = first + c as usize;
            out.flags[i] |= class_flags::GETATTR;
        }
    }
    for row in ff.bindings.range() {
        let s = st.bindings.scope[row];
        let target = fv.binding_target(row);
        let not_call = u32::from(st.bindings.value_kind(row) != ValueKind::Call);
        match target.len() {
            1 => {
                let c = class_of(s);
                if c != NONE {
                    let m = Member { name: target[0], r: row as u32, kind: mkind::VAR };
                    sc.rows.push((c, target[0].to_raw(), mkind::VAR, row as u32, m));
                }
            }
            2 if self_sym == Some(target[0]) && (s as usize) < nd && s != 0 => {
                // `self.x = ...` directly in a method.
                let c = class_of(st.defs.parent[d0 + s as usize]);
                if c != NONE && st.defs.kind[d0 + s as usize] == DefKind::Function as u8 {
                    let m = Member { name: target[1], r: row as u32, kind: mkind::IATTR };
                    sc.rows.push((c, target[1].to_raw(), mkind::IATTR, (not_call << 31) | row as u32, m));
                }
            }
            _ => {}
        }
    }
    sc.rows.sort_unstable_by_key(|&(c, name, kind, key, _)| (c, name, kind, key));
    let mut lens = vec_tail(&mut out.mem_len, count);
    let mut prev: Option<(u32, u32, u8)> = None;
    for &(c, name, kind, _, m) in &sc.rows {
        if prev == Some((c, name, kind)) {
            continue;
        }
        prev = Some((c, name, kind));
        out.members.push(m);
        lens.bump(c as usize);
    }
}

/// `count` zeroed counters appended to `v`.
fn vec_tail(v: &mut Vec<u32>, count: usize) -> Tail<'_> {
    let start = v.len();
    v.resize(start + count, 0);
    Tail { v, start }
}

struct Tail<'a> {
    v: &'a mut Vec<u32>,
    start: usize,
}

impl Tail<'_> {
    #[inline]
    fn bump(&mut self, i: usize) {
        self.v[self.start + i] += 1;
    }
}

/// Sorted `(name, method node)` pairs of every method that is not a dunder, for the
/// unique-method-name fallback and subclass lookups. A name's candidates are one contiguous
/// run, in node order.
#[derive(Debug, Default)]
pub(super) struct MethodIndex {
    pairs: Vec<(u32, NodeId)>,
}

impl MethodIndex {
    pub fn build(ix: &Index) -> MethodIndex {
        let st = ix.facts();
        let (mut pairs, _) = super::namespace::par_concat(ix.file_count() as usize, |f, out| {
            let ff = ix.file_facts(f);
            let d0 = ff.defs.start as usize;
            for row in ff.defs.range().skip(1) {
                let fl = st.defs.flags[row];
                if st.defs.kind[row] == DefKind::Function as u8
                    && fl & def_flags::METHOD != 0
                    && fl & def_flags::DUNDER == 0
                    && let Some(name) = st.defs.name[row]
                {
                    out.push((name.to_raw(), ix.def_node(f, (row - d0) as u32)));
                }
            }
        });
        pairs.par_sort_unstable();
        pairs.shrink_to_fit();
        MethodIndex { pairs }
    }

    /// Method nodes named `name`, in node order.
    #[inline]
    pub fn candidates(&self, name: Sym) -> &[(u32, NodeId)] {
        let k = name.to_raw();
        let i = self.pairs.partition_point(|p| p.0 < k);
        let j = self.pairs[i..].partition_point(|p| p.0 == k) + i;
        &self.pairs[i..j]
    }

    pub fn heap_bytes(&self) -> usize {
        self.pairs.capacity() * size_of::<(u32, NodeId)>()
    }
}
