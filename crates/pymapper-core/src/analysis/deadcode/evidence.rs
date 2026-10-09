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
//! call site is a binary search in the file's own short list of called names.

use super::level;
use crate::extract::facts::evidence_hash;
use crate::index::{FileId, Index, NodeId};
use crate::intern::{FxHashMap, FxHashSet, Sym};
use crate::reach::BitSet;
use crate::resolve::{Imports, Reason, ResolvedCalls};

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

/// Per file, the sorted names explained by its call sites.
struct Explained {
    names: Vec<u32>,
    /// Start of each file's run in `names`, plus the end.
    off: Vec<u32>,
}

impl Explained {
    #[inline]
    fn contains(&self, name: u32, f: FileId) -> bool {
        let run = &self.names[self.off[f as usize] as usize..self.off[f as usize + 1] as usize];
        run.binary_search(&name).is_ok()
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
    /// Per group: distinct files with an unexplained occurrence (saturating at 2) and the first.
    seen: Vec<(u8, FileId)>,
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
        Groups { members, start, seen: vec![(0, 0); n] }
    }

    #[inline]
    fn group(&self, g: u32) -> &[[u32; 4]] {
        &self.members[self.start[g as usize] as usize..self.start[g as usize + 1] as usize]
    }

    /// Records an unexplained occurrence of group `g` in file `f` (each file at most once per
    /// group: evidence hashes are unique per file).
    #[inline]
    fn see(&mut self, g: u32, f: FileId) {
        let s = &mut self.seen[g as usize];
        if s.0 == 0 {
            *s = (1, f);
        } else if s.1 != f {
            s.0 = 2;
        }
    }

    /// Whether group `g` occurs, unexplained, in a file other than `own`.
    #[inline]
    fn elsewhere(&self, g: u32, own: Option<FileId>) -> bool {
        let (n, first) = self.seen[g as usize];
        n >= 2 || (n == 1 && Some(first) != own)
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

/// Runs the scan over the files `included` selects (see the module docs of `deadcode`).
pub(super) fn scan(ix: &Index, imp: &Imports, calls: &ResolvedCalls, included: &[bool], c: &Candidates<'_>) -> Hits {
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
    let (explained, importers) = explained_and_importers(ix, imp, calls, included, c.defs, &def_idx);
    let name_hash = |d: NodeId| evidence_hash(ix.name(d));
    let has = |f: FileId, h: u32| ix.file_view(f).evidence().binary_search(&h).is_ok();
    // Occurrences in the def's own file (its own `__all__` entry is not a use).
    for (i, &d) in c.defs.iter().enumerate() {
        let Some(fd) = ix.file(d) else { continue };
        let s = ix.name_sym(d);
        let module_level = ix.parent(d) == Some(ix.file_node(fd));
        if included[fd as usize]
            && has(fd, name_hash(d))
            && !explained.contains(s.to_raw(), fd)
            && !(module_level && imp.in_all(fd, s))
        {
            hits.defs[i] = level::LINKED;
        }
    }
    // Occurrences in files importing the def directly.
    for &(i, f) in &importers {
        let d = c.defs[i as usize];
        if has(f, name_hash(d)) && !explained.contains(ix.name_sym(d).to_raw(), f) {
            hits.defs[i as usize] = level::LINKED;
        }
    }
    drop(importers);
    aliased_imports(ix, imp, included, &def_idx, &explained, &mut hits.defs);
    let mut map: FxHashMap<u32, Ranges> = FxHashMap::default();
    let module_of = |d: NodeId| ix.file(d).map_or(u32::MAX, |f| ix.file_node(f));
    let defs: Vec<[u32; 4]> =
        c.defs.iter().enumerate().map(|(i, &d)| [name_hash(d), ix.name_sym(d).to_raw(), module_of(d), i as u32]).collect();
    let mut dg = Groups::new(defs, &mut map, 0);
    let mut mg = Groups::new(c.modules.iter().enumerate().map(|(i, &m)| [name_hash(m), 0, 0, i as u32]).collect(), &mut map, 1);
    let by_module = c.imports.iter().enumerate().map(|(i, &(h, f))| [h, 0, ix.file_node(f), i as u32]).collect();
    let ig = Groups::new(by_module, &mut map, 2);
    // Same hashes, so the same group indices as `ig`; keyed by the module's name hash.
    let by_name = c.imports.iter().enumerate().map(|(i, &(h, f))| [h, 0, name_hash(ix.file_node(f)), i as u32]).collect();
    let ig2 = Groups::new(by_name, &mut FxHashMap::default(), 2);
    let mut used = vec![false; ni];
    let mut elsewhere = vec![false; ni];
    for f in 0..ix.file_count() {
        if !included[f as usize] {
            continue;
        }
        let ev = ix.file_view(f).evidence();
        let edges = imp.module_edges(f);
        let imports_module = |m: u32| edges.binary_search_by_key(&m, |e| e.dst).is_ok();
        hits.probes += ev.len() as u64;
        for &h in ev {
            let Some(r) = map.get(&h) else { continue };
            for g in r[0].0..r[0].1 {
                let members = dg.group(g);
                if explained.contains(members[0][1], f) {
                    continue;
                }
                // `import mod` then `mod.name`, or `from pkg import mod`.
                intersect(members, edges.iter().map(|e| e.dst), imports_module, |i| hits.defs[i as usize] = level::LINKED);
                dg.see(g, f);
            }
            for g in r[1].0..r[1].1 {
                mg.see(g, f);
            }
            for g in r[2].0..r[2].1 {
                intersect(ig.group(g), edges.iter().map(|e| e.dst), imports_module, |i| used[i as usize] = true);
                let in_ev = |k: u32| ev.binary_search(&k).is_ok();
                intersect(ig2.group(g), ev.iter().copied(), in_ev, |i| elsewhere[i as usize] = true);
            }
        }
    }
    for g in 0..dg.len() {
        for m in dg.group(g) {
            let i = m[3] as usize;
            if hits.defs[i] < level::LINKED && dg.elsewhere(g, ix.file(c.defs[i])) {
                hits.defs[i] = level::EVIDENCE;
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
        hits.imports[i] = if used[i] {
            import_hit::USED
        } else if elsewhere[i] {
            import_hit::ELSEWHERE
        } else {
            import_hit::NONE
        };
    }
    hits
}

/// Names explained by the call sites of each included file: every call / decorator edge of the
/// file to a def with a candidate's name, and every class of the file whose resolved base has
/// one. Also the `(candidate, file)` pairs of files importing a candidate directly.
fn explained_and_importers(
    ix: &Index,
    imp: &Imports,
    calls: &ResolvedCalls,
    included: &[bool],
    defs: &[NodeId],
    def_idx: &impl Fn(NodeId) -> Option<u32>,
) -> (Explained, Vec<(u32, FileId)>) {
    let mut names: FxHashSet<Sym> = FxHashSet::default();
    for &d in defs {
        names.insert(ix.name_sym(d));
    }
    let cl = calls.classes();
    let files = ix.file_count();
    let mut ex = Explained { names: Vec::new(), off: Vec::with_capacity(files as usize + 1) };
    let mut importers: Vec<(u32, FileId)> = Vec::new();
    for f in 0..files {
        let at = ex.names.len();
        ex.off.push(at as u32);
        if !included[f as usize] {
            continue;
        }
        for e in calls.file_edges(f) {
            // Any edge with a target: the call site explains the name, however sure it is and
            // whichever def of that name it reached (overrides follow their class).
            let s = ix.name_sym(e.dst);
            if e.reason != Reason::Override && names.contains(&s) {
                ex.names.push(s.to_raw());
            }
        }
        for k in cl.file_classes(f) {
            for &b in cl.bases(k) {
                let s = ix.name_sym(cl.node(b));
                if names.contains(&s) {
                    ex.names.push(s.to_raw());
                }
            }
        }
        ex.names[at..].sort_unstable();
        let mut w = at;
        for r in at..ex.names.len() {
            if w == at || ex.names[w - 1] != ex.names[r] {
                ex.names[w] = ex.names[r];
                w += 1;
            }
        }
        ex.names.truncate(w);
        for row in ix.file_facts(f).imports.range() {
            if let Some(t) = imp.target(row)
                && let Some(i) = def_idx(t)
            {
                importers.push((i, f));
            }
        }
    }
    ex.off.push(ex.names.len() as u32);
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
                && !explained.contains(ix.name_sym(t).to_raw(), f)
                && view.evidence().binary_search(&evidence_hash(ix.str(alias))).is_ok()
            {
                out[i as usize] = level::LINKED;
            }
        }
    }
}
