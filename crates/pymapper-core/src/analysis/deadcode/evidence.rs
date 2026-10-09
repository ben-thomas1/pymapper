//! Name-reference fallback (design: dead code (f)): one scan over the evidence hashes of every
//! first-party file, probing the names of the defs, modules and imports still in doubt.
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
//!
//! Every first-party Python file is an evidence source ([`source`]), whether or not it is
//! analysed: tests in directories that are not importable (`tests/my-service/pkg/test_x.py`)
//! or whose dotted path is shadowed by a real module read attributes of production code like
//! any other file. Test code ([`crate::analysis::testfiles::is_test_file`]) is tracked apart, so
//! a name used only by tests is told from one used by production code; stubs only contribute
//! dotted-path string references (a `.pyi` repeats every name of its module).

use std::os::unix::ffi::OsStrExt;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use rayon::prelude::*;

use super::level;
use crate::analysis::testfiles::is_test_file;
use crate::extract::config::ConfigEvidence;
use crate::extract::facts::evidence_hash;
use crate::index::{FileId, Index, NONE, NodeId};
use crate::graph::{Graph, Scc};
use crate::intern::{FxHashMap, Sym};
use crate::reach::BitSet;
use crate::resolve::{Confidence, Imports, Reason, ResolvedCalls};
use crate::roots::Origin;

/// What a file contributes to the scan ([`sources`]).
pub(super) mod source {
    /// Nothing (third-party, stdlib).
    pub const NONE: u8 = 0;
    /// Only string references by dotted path (first-party stubs).
    pub const PATHS: u8 = 1;
    /// All its evidence (first-party `.py`, importable or not).
    pub const CODE: u8 = 2;
    /// All its evidence, as test code.
    pub const TEST: u8 = 3;
}

/// Per file of `ix`: its [`source`] kind. One pass over the file table.
pub(super) fn sources(ix: &Index) -> Vec<u8> {
    (0..ix.file_count())
        .map(|f| {
            if ix.file_origin(f) != Origin::FirstParty {
                source::NONE
            } else if ix.file_path(f).as_os_str().as_bytes().ends_with(b".pyi") {
                source::PATHS
            } else if is_test_file(ix, f) {
                source::TEST
            } else {
                source::CODE
            }
        })
        .collect()
}

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

/// Extra facts about a def or module candidate ([`Hits::def_extra`], [`Hits::mod_extra`]).
pub(super) mod extra {
    /// Defs: the [`super::level::LINKED`] hit came from the def's own file.
    pub const SAME_FILE: u8 = 1;
    /// Its dotted path occurs in a string literal of a first-party file.
    pub const STRING_REF: u8 = 2;
    /// Its dotted path occurs in a config file.
    pub const CONFIG_REF: u8 = 4;
    /// Its name occurs in a config file.
    pub const CONFIG_NAME: u8 = 8;
    /// Defs: its name and its module's dotted path occur in the same config file.
    pub const CONFIG_LINKED: u8 = 16;
}

/// Candidates of the scan. Defs and modules are node ids, ascending.
pub(super) struct Candidates<'a> {
    pub defs: &'a [NodeId],
    pub modules: &'a [NodeId],
    /// `(name hash, file)` of each import candidate.
    pub imports: &'a [(u32, FileId)],
    /// Config-file evidence, if collected.
    pub config: Option<&'a ConfigEvidence>,
}

/// Results, aligned with the candidate slices.
pub(super) struct Hits {
    /// Per def candidate: [`level::LINKED`], [`level::EVIDENCE`] or 0, from non-test files.
    pub defs: Vec<u8>,
    /// Per def candidate: the same from test files ([`source::TEST`]).
    pub def_tests: Vec<u8>,
    /// Per module candidate: the name occurs in another non-test file.
    pub modules: Vec<bool>,
    /// Per module candidate: the name occurs in another test file.
    pub mod_tests: Vec<bool>,
    /// Per import candidate: an [`import_hit`] value.
    pub imports: Vec<u8>,
    /// Per def candidate: [`extra`] bits.
    pub def_extra: Vec<u8>,
    /// Per module candidate: [`extra`] bits.
    pub mod_extra: Vec<u8>,
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

/// Group ranges of each kind for one hash: `[defs, modules, imports, paths]` as
/// `(start, end)` group indices. Path groups hold the dotted paths of def and module
/// candidates ([`MOD_BIT`] marks modules).
type Ranges = [(u32, u32); 4];

/// In path groups: the candidate is a module (index into the module candidates).
const MOD_BIT: u32 = 1 << 31;

/// Members `(hash, name, key, candidate)` sorted, cut into groups of equal `(hash, name)`;
/// within a group members are sorted by `key`.
struct Groups {
    members: Vec<[u32; 4]>,
    /// `start` of each group, plus `members.len()` at the end.
    start: Vec<u32>,
    /// Per group and slot (`seen[g * slots + k]`): [`UNSEEN`], the one file with an
    /// unexplained occurrence, or [`MANY`]. Slot [`CODE_SLOT`] counts non-test files,
    /// [`TEST_SLOT`] test files (groups built with two slots).
    seen: Vec<AtomicU32>,
    slots: u32,
}

/// [`Groups`] occurrence slot of non-test files.
const CODE_SLOT: u32 = 0;
/// [`Groups`] occurrence slot of test files.
const TEST_SLOT: u32 = 1;

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
    /// Groups `members` and records their ranges in slot `kind` of `map`; `slots` occurrence
    /// trackers per group (0 when [`Groups::see`] is never called).
    fn new(mut members: Vec<[u32; 4]>, map: &mut FxHashMap<u32, Ranges>, kind: usize, slots: u32) -> Groups {
        members.sort_unstable();
        let mut start = Vec::new();
        for (i, m) in members.iter().enumerate() {
            if i == 0 || members[i - 1][..2] != m[..2] {
                let g = start.len() as u32;
                start.push(i as u32);
                let r = &mut map.entry(m[0]).or_insert([(0, 0); 4])[kind];
                if r.0 == r.1 {
                    *r = (g, g + 1);
                } else {
                    r.1 = g + 1;
                }
            }
        }
        let n = start.len();
        start.push(members.len() as u32);
        Groups { members, start, seen: (0..n * slots as usize).map(|_| AtomicU32::new(UNSEEN)).collect(), slots }
    }

    #[inline]
    fn group(&self, g: u32) -> &[[u32; 4]] {
        &self.members[self.start[g as usize] as usize..self.start[g as usize + 1] as usize]
    }

    /// Records an unexplained occurrence of group `g` in file `f`, in occurrence slot `k`.
    /// Concurrent calls agree on the result whatever their order: unseen, one file, or many.
    #[inline]
    fn see(&self, g: u32, k: u32, f: FileId) {
        let s = &self.seen[(g * self.slots + k) as usize];
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

    /// Whether group `g` occurs, unexplained, in a file other than `own` (slot `k`).
    #[inline]
    fn elsewhere(&self, g: u32, k: u32, own: Option<FileId>) -> bool {
        let v = self.seen[(g * self.slots + k) as usize].load(Ordering::Relaxed);
        v == MANY || (v != UNSEEN && Some(v) != own)
    }

    fn len(&self) -> u32 {
        self.start.len() as u32 - 1
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

/// Runs the scan over the evidence of the files as `sources` ([`sources`]) classifies them
/// (see the module docs of `deadcode`). `over` is the override graph (base method ->
/// overriding method).
pub(super) fn scan(
    ix: &Index,
    imp: &Imports,
    calls: &ResolvedCalls,
    over: &Graph,
    sources: &[u8],
    c: &Candidates<'_>,
) -> Hits {
    let (nd, nm, ni) = (c.defs.len(), c.modules.len(), c.imports.len());
    let mut hits = Hits {
        defs: vec![0; nd],
        def_tests: vec![0; nd],
        modules: vec![false; nm],
        mod_tests: vec![false; nm],
        imports: vec![import_hit::NONE; ni],
        def_extra: vec![0; nd],
        mod_extra: vec![0; nm],
        probes: 0,
    };
    if nd + nm + ni == 0 {
        return hits;
    }
    // Candidate index per node (4 bytes per node while the scan runs): every call edge is looked
    // up a few times, so O(1) beats a binary search over the candidates.
    let mut cand_of = vec![NONE; ix.node_count() as usize];
    for (i, &d) in c.defs.iter().enumerate() {
        cand_of[d as usize] = i as u32;
    }
    let def_idx = |v: NodeId| -> Option<u32> {
        let i = cand_of[v as usize];
        (i != NONE).then_some(i)
    };
    let (explained, importers) = explained_and_importers(ix, imp, calls, over, sources, c.defs, &def_idx);
    let is_test = |f: FileId| sources[f as usize] == source::TEST;
    let name_hash = |d: NodeId| evidence_hash(ix.name(d));
    let has = |f: FileId, h: u32| ix.file_view(f).evidence().binary_search(&h).is_ok();
    // Occurrences in the def's own file. Its own `__all__` entry counts too: evidence cannot
    // tell that string from a reference such as `TABLE = {"q": decode_q}`.
    for (i, &d) in c.defs.iter().enumerate() {
        let Some(fd) = ix.file(d) else { continue };
        if sources[fd as usize] >= source::CODE && has(fd, name_hash(d)) && !explained.contains(i as u32, fd) {
            hits.defs[i] = level::LINKED;
            hits.def_extra[i] |= extra::SAME_FILE;
        }
    }
    // Occurrences in files importing the def directly.
    for &(i, f) in &importers {
        if has(f, name_hash(c.defs[i as usize])) && !explained.contains(i, f) {
            let out = if is_test(f) { &mut hits.def_tests } else { &mut hits.defs };
            out[i as usize] = level::LINKED;
        }
    }
    drop(importers);
    aliased_imports(ix, imp, sources, &def_idx, &explained, &mut hits.defs, &mut hits.def_tests);
    let mut map: FxHashMap<u32, Ranges> = FxHashMap::default();
    let module_of = |d: NodeId| ix.file(d).map_or(u32::MAX, |f| ix.file_node(f));
    let defs: Vec<[u32; 4]> =
        c.defs.iter().enumerate().map(|(i, &d)| [name_hash(d), ix.name_sym(d).to_raw(), module_of(d), i as u32]).collect();
    let dg = Groups::new(defs, &mut map, 0, 2);
    let mut group_of = vec![0u32; nd];
    for g in 0..dg.len() {
        for m in dg.group(g) {
            group_of[m[3] as usize] = g;
        }
    }
    let mg = Groups::new(c.modules.iter().enumerate().map(|(i, &m)| [name_hash(m), 0, 0, i as u32]).collect(), &mut map, 1, 2);
    let by_module = c.imports.iter().enumerate().map(|(i, &(h, f))| [h, 0, ix.file_node(f), i as u32]).collect();
    let ig = Groups::new(by_module, &mut map, 2, 0);
    // Same hashes, so the same group indices as `ig`; keyed by the module's name hash.
    let by_name = c.imports.iter().enumerate().map(|(i, &(h, f))| [h, 0, name_hash(ix.file_node(f)), i as u32]).collect();
    let ig2 = Groups::new(by_name, &mut FxHashMap::default(), 2, 0);
    // Dotted paths of the candidates (`pkg.mod.Class.method`, `pkg.mod`) and of each def's
    // module: string and config references by path.
    let mut buf = Vec::new();
    let mut path_hash = |v: NodeId| {
        buf.clear();
        ix.qid(v, &mut buf);
        for c in &mut buf {
            if *c == b':' {
                *c = b'.';
            }
        }
        evidence_hash(&buf)
    };
    let mut paths: Vec<[u32; 4]> = Vec::with_capacity(nd + nm);
    let mut def_module_path = Vec::with_capacity(nd);
    for (i, &d) in c.defs.iter().enumerate() {
        paths.push([path_hash(d), 0, 0, i as u32]);
        def_module_path.push(ix.file(d).map_or(0, |f| path_hash(ix.file_node(f))));
    }
    for (i, &m) in c.modules.iter().enumerate() {
        paths.push([path_hash(m), 0, 0, MOD_BIT | i as u32]);
    }
    let pg = Groups::new(paths, &mut map, 3, 0);
    let def_string = AtomicBits::new(nd);
    let mod_string = AtomicBits::new(nm);
    let linked = AtomicBits::new(nd);
    let test_linked = AtomicBits::new(nd);
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
                let src = sources[f as usize];
                if src < source::CODE {
                    // Stubs: their string references by dotted path still count.
                    if src == source::PATHS {
                        let ev = ix.file_view(f).evidence();
                        probes += ev.len() as u64;
                        for &h in ev {
                            let Some(r) = map.get(&h) else { continue };
                            for g in r[3].0..r[3].1 {
                                for m in pg.group(g) {
                                    if m[3] & MOD_BIT != 0 {
                                        mod_string.set(m[3] & !MOD_BIT);
                                    } else {
                                        def_string.set(m[3]);
                                    }
                                }
                            }
                        }
                    }
                    return (probes, partial);
                }
                let (slot, lk) = if src == source::TEST { (TEST_SLOT, &test_linked) } else { (CODE_SLOT, &linked) };
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
                            partial.push((g, slot, f));
                            let link = |i: u32| {
                                if ex.binary_search(&i).is_err() {
                                    lk.set(i);
                                }
                            };
                            intersect(members, edges.iter().map(|e| e.dst), imports_module, link);
                        } else {
                            intersect(members, edges.iter().map(|e| e.dst), imports_module, |i| lk.set(i));
                            dg.see(g, slot, f);
                        }
                    }
                    for g in r[1].0..r[1].1 {
                        mg.see(g, slot, f);
                    }
                    for g in r[2].0..r[2].1 {
                        intersect(ig.group(g), edges.iter().map(|e| e.dst), imports_module, |i| used.set(i));
                        intersect(ig2.group(g), ev.iter().copied(), in_ev, |i| elsewhere.set(i));
                    }
                    for g in r[3].0..r[3].1 {
                        for m in pg.group(g) {
                            if m[3] & MOD_BIT != 0 {
                                mod_string.set(m[3] & !MOD_BIT);
                            } else {
                                def_string.set(m[3]);
                            }
                        }
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
    // Sorted by (group, slot, file): each slot's events of a group are one run.
    partial.sort_unstable();
    let mut pi = 0;
    for g in 0..dg.len() {
        let mut runs = [&partial[..0]; 2];
        for (slot, run) in runs.iter_mut().enumerate() {
            let lo = pi;
            while pi < partial.len() && partial[pi].0 == g && partial[pi].1 == slot as u32 {
                pi += 1;
            }
            *run = &partial[lo..pi];
        }
        for m in dg.group(g) {
            let i = m[3];
            let own = ix.file(c.defs[i as usize]);
            for (slot, lk, out) in [(CODE_SLOT, &linked, &mut hits.defs), (TEST_SLOT, &test_linked, &mut hits.def_tests)] {
                if lk.get(i as usize) {
                    out[i as usize] = level::LINKED;
                } else if out[i as usize] < level::LINKED
                    && (dg.elsewhere(g, slot, own)
                        // Usually the first event qualifies; skipping one means `(i, f)` is
                        // explained or `f` is the def's own file.
                        || runs[slot as usize].iter().any(|&(_, _, f)| Some(f) != own && !explained.contains(i, f)))
                {
                    out[i as usize] = level::EVIDENCE;
                }
                // Test evidence only matters below a production link.
                if slot == CODE_SLOT && out[i as usize] == level::LINKED {
                    break;
                }
            }
        }
    }
    for g in 0..mg.len() {
        for m in mg.group(g) {
            let i = m[3] as usize;
            let own = ix.file(c.modules[i]);
            hits.modules[i] = mg.elsewhere(g, CODE_SLOT, own);
            hits.mod_tests[i] = mg.elsewhere(g, TEST_SLOT, own);
        }
    }
    for i in 0..nd {
        if def_string.get(i) {
            hits.def_extra[i] |= extra::STRING_REF;
        }
    }
    for i in 0..nm {
        if mod_string.get(i) {
            hits.mod_extra[i] |= extra::STRING_REF;
        }
    }
    if let Some(cfg) = c.config {
        // Few files: sequential. Paths, bare names of defs (with their module's path in the
        // same file: linked) and of modules.
        for k in 0..cfg.files() {
            let ev = cfg.file(k);
            hits.probes += ev.len() as u64;
            for &h in ev {
                let Some(r) = map.get(&h) else { continue };
                for g in r[3].0..r[3].1 {
                    for m in pg.group(g) {
                        if m[3] & MOD_BIT != 0 {
                            hits.mod_extra[(m[3] & !MOD_BIT) as usize] |= extra::CONFIG_REF;
                        } else {
                            hits.def_extra[m[3] as usize] |= extra::CONFIG_REF;
                        }
                    }
                }
                for g in r[0].0..r[0].1 {
                    for m in dg.group(g) {
                        let i = m[3] as usize;
                        hits.def_extra[i] |= extra::CONFIG_NAME;
                        if def_module_path[i] != 0 && ev.binary_search(&def_module_path[i]).is_ok() {
                            hits.def_extra[i] |= extra::CONFIG_LINKED;
                        }
                    }
                }
                for g in r[1].0..r[1].1 {
                    for m in mg.group(g) {
                        hits.mod_extra[m[3] as usize] |= extra::CONFIG_NAME;
                    }
                }
            }
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

/// Per evidence file ([`source::CODE`] or [`source::TEST`]), the def candidates whose name
/// occurrences there are explained by call
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
    sources: &[u8],
    defs: &[NodeId],
    def_idx: &(impl Fn(NodeId) -> Option<u32> + Sync),
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
    let cx = FileCx { ix, imp, calls, defs, def_idx, scc: &scc, bases: &bases, is_base: &is_base };
    // Chunks of consecutive files in parallel, concatenated in file order.
    let files = ix.file_count() as usize;
    let chunks: Vec<ChunkOut> = sources
        .par_chunks(CHUNK)
        .enumerate()
        .map(|(c, src)| {
            let (mut cands, mut lens, mut importers, mut unresolved) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
            for (k, &kind) in src.iter().enumerate() {
                let at = cands.len();
                if kind >= source::CODE {
                    cx.file((c * CHUNK + k) as FileId, &mut cands, &mut unresolved, &mut importers);
                }
                lens.push((cands.len() - at) as u32);
            }
            (cands, lens, importers)
        })
        .collect();
    let mut ex = Explained { cands: Vec::new(), off: Vec::with_capacity(files + 1) };
    let mut importers = Vec::new();
    for (cands, lens, mut imps) in chunks {
        let mut at = ex.cands.len() as u32;
        for n in lens {
            ex.off.push(at);
            at += n;
        }
        ex.cands.extend_from_slice(&cands);
        importers.append(&mut imps);
    }
    ex.off.push(ex.cands.len() as u32);
    (ex, importers)
}

/// Files per parallel chunk of [`explained_and_importers`].
const CHUNK: usize = 256;

/// One chunk's explained candidates, run length per file, and direct importers.
type ChunkOut = (Vec<u32>, Vec<u32>, Vec<(u32, FileId)>);

/// Read-only context of the per-file explanation pass.
struct FileCx<'a, F> {
    ix: &'a Index,
    imp: &'a Imports,
    calls: &'a ResolvedCalls,
    defs: &'a [NodeId],
    def_idx: &'a F,
    scc: &'a Scc,
    bases: &'a [(NodeId, u32)],
    is_base: &'a BitSet,
}

impl<F: Fn(NodeId) -> Option<u32>> FileCx<'_, F> {
    /// Appends file `f`'s explained candidates (sorted, unique) to `out` and its direct
    /// importers to `importers`.
    fn file(&self, f: FileId, out: &mut Vec<u32>, unresolved: &mut Vec<Sym>, importers: &mut Vec<(u32, FileId)>) {
        let (ix, def_idx, scc) = (self.ix, self.def_idx, self.scc);
        let at = out.len();
        for e in self.calls.file_edges(f) {
            if e.reason == Reason::Override {
                continue;
            }
            let src = def_idx(e.src);
            let outside = |i: u32| src.is_none_or(|j| scc.comp(j) != scc.comp(i));
            if let Some(i) = def_idx(e.dst)
                && outside(i)
            {
                out.push(i);
            }
            if self.is_base.contains(e.dst) {
                let lo = self.bases.partition_point(|p| p.0 < e.dst);
                for &(b, i) in &self.bases[lo..] {
                    if b != e.dst {
                        break;
                    }
                    if outside(i) {
                        out.push(i);
                    }
                }
            }
        }
        let cl = self.calls.classes();
        for k in cl.file_classes(f) {
            for &b in cl.bases(k) {
                if let Some(i) = def_idx(cl.node(b)) {
                    out.push(i);
                }
            }
        }
        out[at..].sort_unstable();
        let mut w = at;
        for r in at..out.len() {
            if w == at || out[w - 1] != out[r] {
                out[w] = out[r];
                w += 1;
            }
        }
        out.truncate(w);
        if w > at {
            // A call of the same name that did not resolve (ambiguous receiver, parameter,
            // ...) is an occurrence no resolved call explains.
            unresolved.clear();
            let view = ix.file_view(f);
            for row in ix.file_facts(f).calls.range() {
                if self.calls.call(row).0 == Confidence::Unresolved
                    && let Some(&s) = view.call_chain(row).last()
                {
                    unresolved.push(s);
                }
            }
            if !unresolved.is_empty() {
                unresolved.sort_unstable();
                let mut w = at;
                for r in at..out.len() {
                    let i = out[r];
                    if unresolved.binary_search(&ix.name_sym(self.defs[i as usize])).is_err() {
                        out[w] = i;
                        w += 1;
                    }
                }
                out.truncate(w);
            }
        }
        for row in ix.file_facts(f).imports.range() {
            if let Some(t) = self.imp.target(row)
                && let Some(i) = def_idx(t)
            {
                importers.push((i, f));
            }
        }
    }
}

/// Aliased imports of a candidate (`from m import f as g` then `register(g)`): the alias in the
/// importing file's evidence links the def (in `tests` when the file is test code, else in
/// `out`).
fn aliased_imports(
    ix: &Index,
    imp: &Imports,
    sources: &[u8],
    def_idx: &impl Fn(NodeId) -> Option<u32>,
    explained: &Explained,
    out: &mut [u8],
    tests: &mut [u8],
) {
    let st = ix.facts();
    for f in 0..ix.file_count() {
        let src = sources[f as usize];
        if src < source::CODE {
            continue;
        }
        let out = if src == source::TEST { &mut *tests } else { &mut *out };
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
