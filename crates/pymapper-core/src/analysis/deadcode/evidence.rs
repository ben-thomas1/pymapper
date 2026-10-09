//! Name-reference fallback (design: dead code (f)): one scan over the evidence hashes of every
//! included file, probing the names of the defs, modules and imports still in doubt.
//!
//! Candidates are grouped by name, and one hash map from name hash to the groups of each kind
//! answers every probe with one lookup. A probe that hits a group costs O(1) for the group-level
//! fact ("occurs in some other file") plus an intersection bounded by the smaller side: the
//! group's members against the file's module edges (does the file import a member's module?)
//! or against the file's evidence (does it also mention the member's module name?), by binary
//! search into the larger side. So a common name (`get`, `Any`) shared by thousands of
//! candidates and mentioned by thousands of files costs O(files x min(members, edges or
//! evidence of the file) x log), not O(files x members). Whether an occurrence is explained by a
//! call site is a binary search in the file's own short list of explained candidates; a file
//! that explains some member of a hit group is set aside and its members are settled after the
//! scan (the first such file usually decides, so this stays linear in members plus explained
//! pairs).

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use rayon::prelude::*;

use super::level;
use crate::extract::facts::evidence_hash;
use crate::index::{FileId, Index, NodeId};
use crate::graph::{Graph, Scc};
use crate::intern::{FxHashMap, Sym};
use crate::reach::BitSet;
use crate::resolve::{Confidence, Imports, Reason, ResolvedCalls};

/// What the scan found for an import candidate.
pub(super) mod import_hit {
    /// Nothing.
    pub const NONE: u8 = 0;
    /// The name and the module's name occur in another file that does not import the module (a
    /// string reference such as `mock.patch("pkg.mod.name")`).
    pub const ELSEWHERE: u8 = 1;
    /// The name occurs in a file that imports the module (`mod.name`): used.
    pub const USED: u8 = 2;
}

/// Candidates of the scan. Defs and modules are node ids, ascending.
pub(super) struct Candidates<'a> {
    pub defs: &'a [NodeId],
    pub modules: &'a [NodeId],
    /// `(name hash, file)` of each import candidate.
    pub imports: &'a [(u32, FileId)],
}

/// Results, aligned with the candidate slices.
pub(super) struct Hits {
    /// Per def candidate: [`level::LINKED`], [`level::EVIDENCE`] or 0.
    pub defs: Vec<u8>,
    /// Per module candidate: the name occurs in another file.
    pub modules: Vec<bool>,
    /// Per import candidate: an [`import_hit`] value.
    pub imports: Vec<u8>,
    pub probes: u64,
}

/// Per file, the sorted def candidates whose name occurrences there are explained by call
/// sites (see [`explained_and_importers`]).
struct Explained {
    cands: Vec<u32>,
    /// Start of each file's run in `cands`, plus the end.
    off: Vec<u32>,
}

impl Explained {
    #[inline]
    fn run(&self, f: FileId) -> &[u32] {
        &self.cands[self.off[f as usize] as usize..self.off[f as usize + 1] as usize]
    }

    #[inline]
    fn contains(&self, i: u32, f: FileId) -> bool {
        self.run(f).binary_search(&i).is_ok()
    }
}

/// Group ranges of each kind for one name hash: `[defs, modules, imports]` as `(start, end)`
/// group indices.
type Ranges = [(u32, u32); 3];

/// Members `(hash, name, key, candidate)` sorted, cut into groups of equal `(hash, name)`;
/// within a group members are sorted by `key`.
struct Groups {
    members: Vec<[u32; 4]>,
    /// `start` of each group, plus `members.len()` at the end.
    start: Vec<u32>,
    /// Per group: [`UNSEEN`], the one file with an unexplained occurrence, or [`MANY`].
    seen: Vec<AtomicU32>,
}

/// No unexplained occurrence yet.
const UNSEEN: u32 = u32::MAX;
/// Unexplained occurrences in two files or more.
const MANY: u32 = u32::MAX - 1;

/// A bit set written concurrently.
struct AtomicBits(Vec<AtomicU64>);

impl AtomicBits {
    fn new(n: usize) -> AtomicBits {
        AtomicBits((0..n.div_ceil(64)).map(|_| AtomicU64::new(0)).collect())
    }

    #[inline]
    fn set(&self, i: u32) {
        let w = &self.0[(i >> 6) as usize];
        let bit = 1u64 << (i & 63);
        if w.load(Ordering::Relaxed) & bit == 0 {
            w.fetch_or(bit, Ordering::Relaxed);
        }
    }

    #[inline]
    fn get(&self, i: usize) -> bool {
        self.0[i >> 6].load(Ordering::Relaxed) & (1 << (i & 63)) != 0
    }
}

impl Groups {
    /// Groups `members` and records their ranges in slot `kind` of `map`.
    fn new(mut members: Vec<[u32; 4]>, map: &mut FxHashMap<u32, Ranges>, kind: usize) -> Groups {
        members.sort_unstable();
        let mut start = Vec::new();
        for (i, m) in members.iter().enumerate() {
            if i == 0 || members[i - 1][..2] != m[..2] {
                let g = start.len() as u32;
                start.push(i as u32);
                let r = &mut map.entry(m[0]).or_insert([(0, 0); 3])[kind];
                if r.0 == r.1 {
                    *r = (g, g + 1);
                } else {
                    r.1 = g + 1;
                }
            }
        }
        let n = start.len();
        start.push(members.len() as u32);
        Groups { members, start, seen: (0..n).map(|_| AtomicU32::new(UNSEEN)).collect() }
    }

    #[inline]
    fn group(&self, g: u32) -> &[[u32; 4]] {
        &self.members[self.start[g as usize] as usize..self.start[g as usize + 1] as usize]
    }

    /// Records an unexplained occurrence of group `g` in file `f`. Concurrent calls agree on
    /// the result whatever their order: unseen, one file, or many.
    #[inline]
    fn see(&self, g: u32, f: FileId) {
        let s = &self.seen[g as usize];
        let mut cur = s.load(Ordering::Relaxed);
        loop {
            let next = if cur == UNSEEN {
                f
            } else if cur == f || cur == MANY {
                return;
            } else {
                MANY
            };
            match s.compare_exchange_weak(cur, next, Ordering::Relaxed, Ordering::Relaxed) {
                Ok(_) => return,
                Err(v) => cur = v,
            }
        }
    }

    /// Whether group `g` occurs, unexplained, in a file other than `own`.
    #[inline]
    fn elsewhere(&self, g: u32, own: Option<FileId>) -> bool {
        let v = self.seen[g as usize].load(Ordering::Relaxed);
        v == MANY || (v != UNSEEN && Some(v) != own)
    }

    fn len(&self) -> u32 {
        self.seen.len() as u32
    }
}

/// Calls `hit(candidate)` for every member of `members` (sorted by `key`) whose key is in
/// `keys` (sorted ascending), iterating the smaller side; `contains` tests membership in `keys`.
#[inline]
fn intersect(
    members: &[[u32; 4]],
    keys: impl ExactSizeIterator<Item = u32>,
    contains: impl Fn(u32) -> bool,
    mut hit: impl FnMut(u32),
) {
    if members.len() <= keys.len() {
        for m in members {
            if contains(m[2]) {
                hit(m[3]);
            }
        }
    } else {
        for k in keys {
            let lo = members.partition_point(|m| m[2] < k);
            for m in &members[lo..] {
                if m[2] != k {
                    break;
                }
                hit(m[3]);
            }
        }
    }
}

/// Runs the scan over the files `included` selects (see the module docs of `deadcode`). `over`
/// is the override graph (base method -> overriding method).
pub(super) fn scan(
    ix: &Index,
    imp: &Imports,
    calls: &ResolvedCalls,
    over: &Graph,
    included: &[bool],
    c: &Candidates<'_>,
) -> Hits {
    let (nd, nm, ni) = (c.defs.len(), c.modules.len(), c.imports.len());
    let mut hits = Hits { defs: vec![0; nd], modules: vec![false; nm], imports: vec![import_hit::NONE; ni], probes: 0 };
    if nd + nm + ni == 0 {
        return hits;
    }
    let mut is_cand = BitSet::new(ix.node_count());
    for &d in c.defs {
        is_cand.insert(d);
    }
    let def_idx = |v: NodeId| -> Option<u32> {
        if !is_cand.contains(v) {
            return None;
        }
        c.defs.binary_search(&v).ok().map(|i| i as u32)
    };
    let (explained, importers) = explained_and_importers(ix, imp, calls, over, included, c.defs, &def_idx);
    let name_hash = |d: NodeId| evidence_hash(ix.name(d));
    let has = |f: FileId, h: u32| ix.file_view(f).evidence().binary_search(&h).is_ok();
    // Occurrences in the def's own file (its own `__all__` entry is not a use).
    for (i, &d) in c.defs.iter().enumerate() {
        let Some(fd) = ix.file(d) else { continue };
        let s = ix.name_sym(d);
        let module_level = ix.parent(d) == Some(ix.file_node(fd));
        if included[fd as usize]
            && has(fd, name_hash(d))
            && !explained.contains(i as u32, fd)
            && !(module_level && imp.in_all(fd, s))
        {
            hits.defs[i] = level::LINKED;
        }
    }
    // Occurrences in files importing the def directly.
    for &(i, f) in &importers {
        if has(f, name_hash(c.defs[i as usize])) && !explained.contains(i, f) {
            hits.defs[i as usize] = level::LINKED;
        }
    }
    drop(importers);
    aliased_imports(ix, imp, included, &def_idx, &explained, &mut hits.defs);
    let mut map: FxHashMap<u32, Ranges> = FxHashMap::default();
    let module_of = |d: NodeId| ix.file(d).map_or(u32::MAX, |f| ix.file_node(f));
    let defs: Vec<[u32; 4]> =
        c.defs.iter().enumerate().map(|(i, &d)| [name_hash(d), ix.name_sym(d).to_raw(), module_of(d), i as u32]).collect();
    let dg = Groups::new(defs, &mut map, 0);
    let mut group_of = vec![0u32; nd];
    for g in 0..dg.len() {
        for m in dg.group(g) {
            group_of[m[3] as usize] = g;
        }
    }
    let mg = Groups::new(c.modules.iter().enumerate().map(|(i, &m)| [name_hash(m), 0, 0, i as u32]).collect(), &mut map, 1);
    let by_module = c.imports.iter().enumerate().map(|(i, &(h, f))| [h, 0, ix.file_node(f), i as u32]).collect();
    let ig = Groups::new(by_module, &mut map, 2);
    // Same hashes, so the same group indices as `ig`; keyed by the module's name hash.
    let by_name = c.imports.iter().enumerate().map(|(i, &(h, f))| [h, 0, name_hash(ix.file_node(f)), i as u32]).collect();
    let ig2 = Groups::new(by_name, &mut FxHashMap::default(), 2);
    let linked = AtomicBits::new(nd);
    let used = AtomicBits::new(ni);
    let elsewhere = AtomicBits::new(ni);
    // Parallel over files: every write is an idempotent atomic update (see `Groups::see`), so
    // the result does not depend on scheduling. Files that explain some member of a hit group
    // are returned as `(group, file)` for a per-member pass.
    let (probes, mut partial) = (0..ix.file_count())
        .into_par_iter()
        .with_min_len(64)
        .fold(
            || (0u64, Vec::new()),
            |(mut probes, mut partial), f| {
                if !included[f as usize] {
                    return (probes, partial);
                }
                let ev = ix.file_view(f).evidence();
                let edges = imp.module_edges(f);
                let ex = explained.run(f);
                let imports_module = |m: u32| edges.binary_search_by_key(&m, |e| e.dst).is_ok();
                let in_ev = |k: u32| ev.binary_search(&k).is_ok();
                probes += ev.len() as u64;
                for &h in ev {
                    let Some(r) = map.get(&h) else { continue };
                    for g in r[0].0..r[0].1 {
                        let members = dg.group(g);
                        // `import mod` then `mod.name`, or `from pkg import mod`.
                        if ex.iter().any(|&i| group_of[i as usize] == g) {
                            partial.push((g, f));
                            let link = |i: u32| {
                                if ex.binary_search(&i).is_err() {
                                    linked.set(i);
                                }
                            };
                            intersect(members, edges.iter().map(|e| e.dst), imports_module, link);
                        } else {
                            intersect(members, edges.iter().map(|e| e.dst), imports_module, |i| linked.set(i));
                            dg.see(g, f);
                        }
                    }
                    for g in r[1].0..r[1].1 {
                        mg.see(g, f);
                    }
                    for g in r[2].0..r[2].1 {
                        intersect(ig.group(g), edges.iter().map(|e| e.dst), imports_module, |i| used.set(i));
                        intersect(ig2.group(g), ev.iter().copied(), in_ev, |i| elsewhere.set(i));
                    }
                }
                (probes, partial)
            },
        )
        .reduce(
            || (0u64, Vec::new()),
            |(p1, mut v1), (p2, mut v2)| {
                v1.append(&mut v2);
                (p1 + p2, v1)
            },
        );
    hits.probes = probes;
    partial.sort_unstable();
    let mut pi = 0;
    for g in 0..dg.len() {
        let lo = pi;
        while pi < partial.len() && partial[pi].0 == g {
            pi += 1;
        }
        let events = &partial[lo..pi];
        for m in dg.group(g) {
            let i = m[3];
            let own = ix.file(c.defs[i as usize]);
            if linked.get(i as usize) {
                hits.defs[i as usize] = level::LINKED;
            } else if hits.defs[i as usize] < level::LINKED
                && (dg.elsewhere(g, own)
                    // Usually the first event qualifies; skipping one means `(i, f)` is
                    // explained or `f` is the def's own file.
                    || events.iter().any(|&(_, f)| Some(f) != own && !explained.contains(i, f)))
            {
                hits.defs[i as usize] = level::EVIDENCE;
            }
        }
    }
    for g in 0..mg.len() {
        for m in mg.group(g) {
            let i = m[3] as usize;
            hits.modules[i] = mg.elsewhere(g, ix.file(c.modules[i]));
        }
    }
    for i in 0..ni {
        hits.imports[i] = if used.get(i) {
            import_hit::USED
        } else if elsewhere.get(i) {
            import_hit::ELSEWHERE
        } else {
            import_hit::NONE
        };
    }
    hits
}

/// Per included file, the def candidates whose name occurrences there are explained by call
/// sites: a call or decorator edge of the file to the def, or to a base method the def
/// overrides (dynamic dispatch), unless the caller is a candidate in the def's own call cycle
/// (recursion does not explain a reference made elsewhere in the file); and a class of the
/// file whose resolved base is the def. A candidate is not explained in a file that also has
/// an unresolved call of its name. Also the `(candidate, file)` pairs of files importing a
/// candidate directly.
fn explained_and_importers(
    ix: &Index,
    imp: &Imports,
    calls: &ResolvedCalls,
    over: &Graph,
    included: &[bool],
    defs: &[NodeId],
    def_idx: &impl Fn(NodeId) -> Option<u32>,
) -> (Explained, Vec<(u32, FileId)>) {
    let edges = calls.edges();
    // Call cycles among the candidates.
    let cg = Graph::build(defs.len() as u32, || {
        edges.iter().filter(|e| e.reason != Reason::Override).filter_map(|e| Some((def_idx(e.src)?, def_idx(e.dst)?)))
    });
    let scc = Scc::of(&cg);
    drop(cg);
    // Base methods whose calls may dispatch to a candidate override: (base, candidate).
    let mut bases: Vec<(NodeId, u32)> = Vec::new();
    let mut is_base = BitSet::new(ix.node_count());
    for (i, &d) in defs.iter().enumerate() {
        for &b in over.pred(d) {
            bases.push((b, i as u32));
            is_base.insert(b);
        }
    }
    bases.sort_unstable();
    let cl = calls.classes();
    let files = ix.file_count();
    let mut ex = Explained { cands: Vec::new(), off: Vec::with_capacity(files as usize + 1) };
    let mut importers: Vec<(u32, FileId)> = Vec::new();
    let mut unresolved: Vec<Sym> = Vec::new();
    for f in 0..files {
        let at = ex.cands.len();
        ex.off.push(at as u32);
        if !included[f as usize] {
            continue;
        }
        for e in calls.file_edges(f) {
            if e.reason == Reason::Override {
                continue;
            }
            let src = def_idx(e.src);
            let outside = |i: u32| src.is_none_or(|j| scc.comp(j) != scc.comp(i));
            if let Some(i) = def_idx(e.dst)
                && outside(i)
            {
                ex.cands.push(i);
            }
            if is_base.contains(e.dst) {
                let lo = bases.partition_point(|p| p.0 < e.dst);
                for &(b, i) in &bases[lo..] {
                    if b != e.dst {
                        break;
                    }
                    if outside(i) {
                        ex.cands.push(i);
                    }
                }
            }
        }
        for k in cl.file_classes(f) {
            for &b in cl.bases(k) {
                if let Some(i) = def_idx(cl.node(b)) {
                    ex.cands.push(i);
                }
            }
        }
        ex.cands[at..].sort_unstable();
        let mut w = at;
        for r in at..ex.cands.len() {
            if w == at || ex.cands[w - 1] != ex.cands[r] {
                ex.cands[w] = ex.cands[r];
                w += 1;
            }
        }
        ex.cands.truncate(w);
        if w > at {
            // A call of the same name that did not resolve (ambiguous receiver, parameter,
            // ...) is an occurrence no resolved call explains.
            unresolved.clear();
            let view = ix.file_view(f);
            for row in ix.file_facts(f).calls.range() {
                if calls.call(row).0 == Confidence::Unresolved
                    && let Some(&s) = view.call_chain(row).last()
                {
                    unresolved.push(s);
                }
            }
            if !unresolved.is_empty() {
                unresolved.sort_unstable();
                let mut w = at;
                for r in at..ex.cands.len() {
                    let i = ex.cands[r];
                    if unresolved.binary_search(&ix.name_sym(defs[i as usize])).is_err() {
                        ex.cands[w] = i;
                        w += 1;
                    }
                }
                ex.cands.truncate(w);
            }
        }
        for row in ix.file_facts(f).imports.range() {
            if let Some(t) = imp.target(row)
                && let Some(i) = def_idx(t)
            {
                importers.push((i, f));
            }
        }
    }
    ex.off.push(ex.cands.len() as u32);
    (ex, importers)
}

/// Aliased imports of a candidate (`from m import f as g` then `register(g)`): the alias in the
/// importing file's evidence links the def.
fn aliased_imports(
    ix: &Index,
    imp: &Imports,
    included: &[bool],
    def_idx: &impl Fn(NodeId) -> Option<u32>,
    explained: &Explained,
    out: &mut [u8],
) {
    let st = ix.facts();
    for f in 0..ix.file_count() {
        if !included[f as usize] {
            continue;
        }
        let view = ix.file_view(f);
        for row in ix.file_facts(f).imports.range() {
            let (Some(alias), Some(t)) = (st.imports.alias[row], imp.target(row)) else { continue };
            if let Some(i) = def_idx(t)
                && Some(alias) != st.imports.name[row]
                && !explained.contains(i, f)
                && view.evidence().binary_search(&evidence_hash(ix.str(alias))).is_ok()
            {
                out[i as usize] = level::LINKED;
            }
        }
    }
}
