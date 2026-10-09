//! Name-reference fallback (design: dead code (f)): one scan over the evidence hashes of every
//! included file, probing the names of the defs, modules and imports still in doubt.

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

/// Sorted `(candidate, file)` pairs with a membership test.
struct PairSet(Vec<(u32, FileId)>);

impl PairSet {
    fn new(mut v: Vec<(u32, FileId)>) -> PairSet {
        v.sort_unstable();
        v.dedup();
        PairSet(v)
    }

    #[inline]
    fn contains(&self, c: u32, f: FileId) -> bool {
        self.0.binary_search(&(c, f)).is_ok()
    }
}

/// Whether file `f` imports module node `m` (a module edge `f -> m`).
#[inline]
fn imports_module(imp: &Imports, f: FileId, m: NodeId) -> bool {
    imp.module_edges(f).binary_search_by_key(&m, |e| e.dst).is_ok()
}

/// Runs the scan over the files `included` selects (see the module docs of `deadcode`).
pub(super) fn scan(ix: &Index, imp: &Imports, calls: &ResolvedCalls, included: &[bool], c: &Candidates<'_>) -> Hits {
    let nd = c.defs.len();
    let nm = c.modules.len();
    let mut hits =
        Hits { defs: vec![0; nd], modules: vec![false; nm], imports: vec![import_hit::NONE; c.imports.len()], probes: 0 };
    if nd + nm + c.imports.len() == 0 {
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
    let files = ix.file_count();
    // Names of the def candidates: occurrences of these are explained by resolved call sites.
    let mut names: FxHashSet<Sym> = FxHashSet::default();
    for &d in c.defs {
        names.insert(ix.name_sym(d));
    }
    // Occurrences explained by call sites: `(name, file)` for every call / decorator edge of a
    // file to a def with a candidate's name, and every class of a file whose resolved base has
    // one.
    let mut explained: Vec<(u32, FileId)> = Vec::new();
    // Files importing a def candidate directly.
    let mut importers: Vec<(u32, FileId)> = Vec::new();
    for f in 0..files {
        if !included[f as usize] {
            continue;
        }
        let at = explained.len();
        for e in calls.file_edges(f) {
            // Any edge with a target: the call site explains the name, however sure it is and
            // whichever def of that name it reached (overrides follow their class).
            let s = ix.name_sym(e.dst);
            if e.reason != Reason::Override && names.contains(&s) {
                explained.push((s.to_raw(), f));
            }
        }
        // At most one entry per (name, file).
        explained[at..].sort_unstable();
        let mut w = at;
        for r in at..explained.len() {
            if w == at || explained[w - 1] != explained[r] {
                explained[w] = explained[r];
                w += 1;
            }
        }
        explained.truncate(w);
        for row in ix.file_facts(f).imports.range() {
            if let Some(t) = imp.target(row)
                && let Some(i) = def_idx(t)
            {
                importers.push((i, f));
            }
        }
    }
    let cl = calls.classes();
    for k in 0..cl.len() {
        let Some(f) = ix.file(cl.node(k)) else { continue };
        if !included[f as usize] {
            continue;
        }
        for &b in cl.bases(k) {
            let s = ix.name_sym(cl.node(b));
            if names.contains(&s) {
                explained.push((s.to_raw(), f));
            }
        }
    }
    drop(names);
    let explained = PairSet::new(explained);
    let importers = PairSet::new(importers);
    // Aliased imports of a candidate (`from m import f as g` then `register(g)`).
    let st = ix.facts();
    for f in 0..files {
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
                hits.defs[i as usize] = level::LINKED;
            }
        }
    }
    // Hash -> candidate entries: defs `0..nd`, modules `nd..nd + nm`, imports after.
    let mut keys: Vec<(u32, u32)> = Vec::with_capacity(nd + nm + c.imports.len());
    for (i, &d) in c.defs.iter().enumerate() {
        keys.push((evidence_hash(ix.name(d)), i as u32));
    }
    for (i, &m) in c.modules.iter().enumerate() {
        keys.push((evidence_hash(ix.name(m)), (nd + i) as u32));
    }
    for (i, &(h, _)) in c.imports.iter().enumerate() {
        keys.push((h, (nd + nm + i) as u32));
    }
    keys.sort_unstable();
    let mut first: FxHashMap<u32, u32> = FxHashMap::default();
    first.reserve(keys.len());
    for (i, &(h, _)) in keys.iter().enumerate().rev() {
        first.insert(h, i as u32);
    }
    for f in 0..files {
        if !included[f as usize] {
            continue;
        }
        let ev = ix.file_view(f).evidence();
        hits.probes += ev.len() as u64;
        for &h in ev {
            let Some(&s) = first.get(&h) else { continue };
            for &(kh, e) in &keys[s as usize..] {
                if kh != h {
                    break;
                }
                let e = e as usize;
                if e < nd {
                    def_hit(ix, imp, c.defs[e], e as u32, f, &explained, &importers, &mut hits.defs[e]);
                } else if e < nd + nm {
                    let m = c.modules[e - nd];
                    if ix.file(m) != Some(f) {
                        hits.modules[e - nd] = true;
                    }
                } else {
                    let i = e - nd - nm;
                    let file = c.imports[i].1;
                    if file != f && hits.imports[i] != import_hit::USED {
                        let m = ix.file_node(file);
                        let hit = if imports_module(imp, f, m) {
                            import_hit::USED
                        } else if ev.binary_search(&evidence_hash(ix.name(m))).is_ok() {
                            // Also mentions the module: a dotted string such as "pkg.mod.name".
                            import_hit::ELSEWHERE
                        } else {
                            import_hit::NONE
                        };
                        hits.imports[i] = hits.imports[i].max(hit);
                    }
                }
            }
        }
    }
    hits
}

/// An occurrence of def candidate `d` (index `i`) in file `f`.
#[allow(clippy::too_many_arguments)]
#[inline]
fn def_hit(
    ix: &Index,
    imp: &Imports,
    d: NodeId,
    i: u32,
    f: FileId,
    explained: &PairSet,
    importers: &PairSet,
    out: &mut u8,
) {
    if *out >= level::LINKED || explained.contains(ix.name_sym(d).to_raw(), f) {
        return;
    }
    let Some(fd) = ix.file(d) else { return };
    let lvl = if fd == f {
        // The def's own `__all__` entry is not a use.
        let module_level = ix.parent(d) == Some(ix.file_node(fd));
        if module_level && imp.in_all(f, ix.name_sym(d)) {
            return;
        }
        level::LINKED
    } else if importers.contains(i, f) || imports_module(imp, f, ix.file_node(fd)) {
        level::LINKED
    } else {
        level::EVIDENCE
    };
    *out = (*out).max(lvl);
}
