//! Layering contract analysis (design: Goals; rules PM302 `layer-violation`, PM303
//! `forbidden-import`, PM304 `independence-violation`).
//!
//! import-linter-style contracts from `[[contracts]]` ([`Contract`]), checked against every
//! import row of the index:
//!
//! ```text
//!   contracts ──> compile        per contract: module names -> ModId (trie lookups) -> a role
//!                 (compile.rs)   (slot) per module of the module space (nearest listed
//!                                ancestor-or-self), forbidden names that are not loaded,
//!                                ignore-imports patterns
//!             ──> reach          transitive contracts only: reverse multi-source BFS over the
//!                 (reach.rs)     import graph through unlisted modules
//!             ──> check          per file in parallel: rows -> imported module (as the module
//!                                graph sees it) -> role lookups -> findings in file order
//! ```
//!
//! # Semantics
//!
//! - A contract module covers its submodules; a module gets the role of its nearest listed
//!   ancestor-or-self. Imports are checked as the module graph sees them (see
//!   [`row_module`](super::cycles::row_module)): `from pkg import sub` imports `pkg.sub`, a
//!   symbol import imports the module it is imported from.
//! - `layers`: highest layer first. A module of layer `i` importing a module of layer `j < i`
//!   of the same container is a PM302. With `independent-siblings`, importing a sibling of the
//!   same layer is a PM304. With `containers`, each container has its own layers (named
//!   `container.layer`) and imports between containers are not checked.
//! - `forbidden`: a `source` module importing a `forbidden` module is a PM303. Forbidden
//!   modules that are not loaded (third-party, stdlib) match unresolved absolute imports by
//!   dotted prefix (`import a.b.c` and `from a.b import c` both match `a.b`).
//! - `independence`: a module of one entry importing a module of another is a PM304.
//! - `allow-type-checking` (default on) exempts `if TYPE_CHECKING:` imports; `ignore-imports`
//!   exempts `importer -> imported` pairs (exact dotted names; `*` one segment, `**` zero or
//!   more). A file importing its own module and stub-shadowed `.py` files count, shadowed and
//!   non-importable files do not.
//! - `transitive`: also an import of an unlisted module that reaches a violating module
//!   through unlisted modules only, reported at that import with the module reached (chains
//!   through other listed modules are reported where they leave a listed module).
//! - One finding per violating import statement and imported module: rows of one statement
//!   (`from m import a, b`) are reported once.
//!
//! # Cost class: O(V + E)
//!
//! Compilation is O(modules x depth) hash lookups per contract plus one role array (`4`
//! bytes per module, two for `forbidden`). The check reads every import row once (one binary
//! search for the imported module) and costs O(1) per row and contract; files whose module has
//! no source role in any contract are skipped without reading their rows. Transitive
//! contracts build the import graph once (shared by contracts with the same
//! `allow-type-checking`) and run one linear BFS per contract, per container for layers
//! (`4` bytes per module and container, `8` for independence). Nothing is allocated per row;
//! dotted names are only built to check `ignore-imports` on violations.

mod compile;
mod reach;
#[cfg(test)]
mod tests;

pub use compile::MissingModule;

use rayon::prelude::*;

use compile::{Compiled, ExtName, Kind, NO_ROLE, Space, compile, pattern_matches};
use reach::Reach;

use super::cycles::row_module;
use crate::config::Contract;
use crate::graph::{ModuleGraph, import_graph_where};
use crate::index::{FileId, Index, NONE};
use crate::intern::Sym;
use crate::resolve::{Imports, ModuleTable};
use crate::roots::FileStatus;
use crate::rules::codes::{FORBIDDEN_IMPORT, INDEPENDENCE_VIOLATION, LAYER_VIOLATION};
use crate::rules::{self, Arg, Findings, Location, Rule, SymbolId};

/// Files per parallel work unit of the check.
const CHUNK: usize = 256;

/// Outcome of [`analyse`] besides the findings.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LayersReport {
    /// Contract module names that match nothing (a typo, or code that is not analysed): the
    /// contract silently checks less than written, so callers should warn.
    pub missing: Vec<MissingModule>,
    /// Import rows of files with a source role in some contract.
    pub rows_checked: u64,
    /// Findings emitted.
    pub violations: u64,
    /// Peak heap bytes of the compiled contracts and transitive tables.
    pub heap_bytes: usize,
}

/// Whether the imports of file `f` are checked: importable files (stubs and stub-shadowed
/// implementations included).
#[inline]
pub fn contract_file(ix: &Index, f: FileId) -> bool {
    matches!(ix.file_status(f), FileStatus::Active | FileStatus::StubShadowed)
}

/// Checks every contract against the imports of `ix` and appends the findings to `out`, in
/// file order (see the module docs). `contracts` is the config list: findings refer to
/// contracts by their index in it.
pub fn analyse(ix: &Index, imp: &Imports, contracts: &[Contract], out: &mut Findings) -> LayersReport {
    let mut report = LayersReport::default();
    if contracts.is_empty() {
        return report;
    }
    let table = imp.modules();
    let space = Space::build(ix, table);
    let compiled: Vec<Compiled> = contracts
        .iter()
        .enumerate()
        .map(|(i, c)| compile(ix, table, &space, i as u32, c, &mut report.missing))
        .filter(Compiled::any_source)
        .collect();
    if compiled.is_empty() {
        return report;
    }
    // Transitive tables; import graphs shared per exclude mask.
    let mut graphs: [Option<ModuleGraph>; 2] = [None, None];
    let mut reach: Vec<Option<Reach>> = Vec::with_capacity(compiled.len());
    for c in &compiled {
        if !c.transitive {
            reach.push(None);
            continue;
        }
        let slot = usize::from(c.exclude != 0);
        let mg = graphs[slot].get_or_insert_with(|| import_graph_where(ix, imp, c.exclude, |f| contract_file(ix, f)));
        let g = mg.graph();
        reach.push(Some(match c.kind {
            Kind::Layers { .. } => reach::layers(c, g),
            Kind::Independence => reach::independence(c, g),
            Kind::Forbidden => {
                let seeds = ext_seeds(ix, imp, &space, c);
                reach::forbidden(c, g, &seeds, |v| module_name(ix, table, &space, v))
            }
        }));
    }
    let heap = space.nodes.capacity() * 8
        + compiled.iter().map(|c| (c.src.capacity() + c.dst.capacity()) * 4).sum::<usize>()
        + reach.iter().flatten().map(Reach::heap_bytes).sum::<usize>()
        + graphs.iter().flatten().map(ModuleGraph::heap_bytes).sum::<usize>();
    drop(graphs);
    let cx = Cx { ix, imp, table, space: &space, compiled: &compiled, reach: &reach };
    let files = ix.file_count() as usize;
    let chunks: Vec<(Findings, u64)> = (0..files.div_ceil(CHUNK))
        .into_par_iter()
        .map(|k| {
            let mut sc = Scratch::default();
            let mut buf = Findings::new();
            let mut rows = 0u64;
            for f in k * CHUNK..((k + 1) * CHUNK).min(files) {
                rows += cx.check_file(f as FileId, &mut sc, &mut buf);
            }
            (buf, rows)
        })
        .collect();
    let before = out.len();
    for (mut buf, rows) in chunks {
        report.rows_checked += rows;
        out.append(&mut buf);
    }
    report.violations = (out.len() - before) as u64;
    report.heap_bytes = heap;
    report
}

/// Raw `Sym` naming module `v` (local id) in PM303 messages: the file's dotted module name,
/// else (namespace package) the dotted name if interned, else its last segment.
fn module_name(ix: &Index, table: &ModuleTable, space: &Space, v: u32) -> u32 {
    let node = space.nodes[v as usize];
    if let Some(f) = ix.file(node) {
        return ix.file_module(f).to_raw();
    }
    let m = space.mods[v as usize];
    if m != NONE {
        let mut buf = Vec::new();
        table.dotted(ix, m, &mut buf);
        if let Some(s) = ix.strings().get(&buf) {
            return s.to_raw();
        }
    }
    ix.name_sym(node).to_raw()
}

/// The not-loaded forbidden name import `row` of file `f` matches, if any: an absolute,
/// unresolved import whose dotted module (or module plus imported name) starts with the name.
fn match_ext<'a>(ix: &Index, imp: &Imports, ext: &'a [ExtName], f: FileId, row: usize) -> Option<&'a ExtName> {
    let st = ix.facts();
    if ext.is_empty() || st.imports.level[row] != 0 || !imp.status(row).is_external() {
        return None;
    }
    let segs = ix.file_view(f).import_module(row);
    let name = st.imports.name[row];
    ext.iter().find(|e| {
        let k = e.segs.len();
        if k <= segs.len() {
            segs[..k] == *e.segs
        } else {
            k == segs.len() + 1 && segs == &e.segs[..k - 1] && name == Some(e.segs[k - 1])
        }
    })
}

/// Unlisted modules importing a forbidden module that is not loaded (module, name), for a
/// transitive `forbidden` contract.
fn ext_seeds(ix: &Index, imp: &Imports, space: &Space, c: &Compiled) -> Vec<(u32, u32)> {
    let mut out = Vec::new();
    if c.ext.is_empty() {
        return out;
    }
    for f in 0..ix.file_count() {
        if !contract_file(ix, f) {
            continue;
        }
        let v = space.local(ix.file_node(f));
        if v == NONE || c.src[v as usize] != NO_ROLE {
            continue;
        }
        for row in ix.file_facts(f).imports.range() {
            if imp.flags(row) & c.exclude != 0 {
                continue;
            }
            if let Some(e) = match_ext(ix, imp, &c.ext, f, row) {
                out.push((v, e.display));
                break;
            }
        }
    }
    out
}

/// Per-chunk scratch of the check.
#[derive(Default)]
struct Scratch {
    /// Contracts (indices into the compiled list) in which the current file has a role.
    active: Vec<usize>,
    /// Last finding of each active contract in the current file.
    last: Vec<Option<Last>>,
    /// Dotted names for `ignore-imports` checks.
    importer: Vec<u8>,
    imported: Vec<u8>,
}

/// Read-only context of the check.
struct Cx<'a> {
    ix: &'a Index,
    imp: &'a Imports,
    table: &'a ModuleTable,
    space: &'a Space,
    compiled: &'a [Compiled],
    reach: &'a [Option<Reach>],
}

/// What a violating row imports, for the message.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Target {
    /// A module (local id).
    Module(u32),
    /// A forbidden module name (raw `Sym`).
    Name(u32),
}

/// The last finding emitted for the current file, to report a statement's rows once.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Last {
    contract: u32,
    rule: Rule,
    line: u32,
    target: Target,
}

impl Cx<'_> {
    /// Checks the import rows of file `f`; returns the number of rows read.
    fn check_file(&self, f: FileId, sc: &mut Scratch, out: &mut Findings) -> u64 {
        let ix = self.ix;
        if !contract_file(ix, f) {
            return 0;
        }
        let src_node = ix.file_node(f);
        let s = self.space.local(src_node);
        if s == NONE {
            return 0;
        }
        sc.active.clear();
        for (i, c) in self.compiled.iter().enumerate() {
            if c.src[s as usize] != NO_ROLE {
                sc.active.push(i);
            }
        }
        if sc.active.is_empty() {
            return 0;
        }
        sc.last.clear();
        sc.last.resize(sc.active.len(), None);
        let st = ix.facts();
        let rows = ix.file_facts(f).imports.range();
        let n = rows.len() as u64;
        for row in rows {
            let dst_node = row_module(self.imp, row);
            if dst_node == src_node {
                continue;
            }
            let d = self.space.local(dst_node);
            let flags = self.imp.flags(row);
            for k in 0..sc.active.len() {
                let ci = sc.active[k];
                let c = &self.compiled[ci];
                if flags & c.exclude != 0 {
                    continue;
                }
                let Some((rule, target)) = self.violation(ci, f, row, s, d) else { continue };
                let line = st.imports.line[row];
                let now = Last { contract: c.index, rule, line, target };
                if sc.last[k] == Some(now) || self.ignored(c, f, row, s, d, sc) {
                    continue;
                }
                sc.last[k] = Some(now);
                let to = match target {
                    Target::Module(v) => Arg::Symbol(SymbolId(self.space.nodes[v as usize])),
                    Target::Name(raw) => Arg::Name(raw),
                };
                let loc = Location::new(rules::FileId(f), line, st.imports.col[row] + 1);
                out.push(rule, loc, SymbolId(src_node), 100, &[Arg::Symbol(SymbolId(src_node)), to, Arg::Contract(c.index)]);
            }
        }
        n
    }

    /// The rule broken by row `row` of file `f` (importer `s`, imported module `d`, `NONE` if
    /// not loaded) under compiled contract `ci`, and what to name as imported.
    fn violation(&self, ci: usize, f: FileId, row: usize, s: u32, d: u32) -> Option<(Rule, Target)> {
        let c = &self.compiled[ci];
        let reach = self.reach[ci].as_ref();
        let ss = c.slots[c.src[s as usize] as usize];
        match c.kind {
            Kind::Layers { siblings, .. } => {
                if d == NONE {
                    return None;
                }
                let dr = c.src[d as usize];
                if dr != NO_ROLE {
                    let ds = c.slots[dr as usize];
                    if ds.group != ss.group {
                        return None;
                    }
                    if ds.layer < ss.layer {
                        return Some((LAYER_VIOLATION, Target::Module(d)));
                    }
                    if siblings && ds.layer == ss.layer && ds.member != ss.member {
                        return Some((INDEPENDENCE_VIOLATION, Target::Module(d)));
                    }
                    return None;
                }
                let h = reach?.layer_hit(ss.group, d);
                (h != NONE && c.slots[c.src[h as usize] as usize].layer < ss.layer)
                    .then_some((LAYER_VIOLATION, Target::Module(h)))
            }
            Kind::Independence => {
                if d == NONE {
                    return None;
                }
                let dr = c.src[d as usize];
                if dr != NO_ROLE {
                    return (c.slots[dr as usize].member != ss.member)
                        .then_some((INDEPENDENCE_VIOLATION, Target::Module(d)));
                }
                let h = reach?.independence_hit(c, d, ss.member);
                (h != NONE).then_some((INDEPENDENCE_VIOLATION, Target::Module(h)))
            }
            Kind::Forbidden => {
                if d == NONE {
                    return match_ext(self.ix, self.imp, &c.ext, f, row)
                        .map(|e| (FORBIDDEN_IMPORT, Target::Name(e.display)));
                }
                if c.dst[d as usize] != NO_ROLE {
                    let name = module_name(self.ix, self.table, self.space, d);
                    return Some((FORBIDDEN_IMPORT, Target::Name(name)));
                }
                if c.src[d as usize] != NO_ROLE {
                    return None;
                }
                let name = reach?.forbidden_hit(d);
                (name != NONE).then_some((FORBIDDEN_IMPORT, Target::Name(name)))
            }
        }
    }

    /// Whether an `ignore-imports` pattern of `c` exempts the import of `d` (or, when not
    /// loaded, of the row's dotted module) by `s`.
    fn ignored(&self, c: &Compiled, f: FileId, row: usize, s: u32, d: u32, sc: &mut Scratch) -> bool {
        if c.ignore.is_empty() {
            return false;
        }
        let ix = self.ix;
        sc.importer.clear();
        let sm = self.space.mods[s as usize];
        if sm != NONE {
            self.table.dotted(ix, sm, &mut sc.importer);
        }
        sc.imported.clear();
        let dm = if d == NONE { NONE } else { self.space.mods[d as usize] };
        if dm != NONE {
            self.table.dotted(ix, dm, &mut sc.imported);
        } else {
            let segs: &[Sym] = ix.file_view(f).import_module(row);
            for (i, &seg) in segs.iter().enumerate() {
                if i > 0 {
                    sc.imported.push(b'.');
                }
                sc.imported.extend_from_slice(ix.str(seg));
            }
        }
        // Only reached for violations (rare): two small vectors per check are fine.
        let importer: Vec<&[u8]> = sc.importer.split(|&b| b == b'.').collect();
        let imported: Vec<&[u8]> = sc.imported.split(|&b| b == b'.').collect();
        c.ignore.iter().any(|p| pattern_matches(&p.importer, &importer) && pattern_matches(&p.imported, &imported))
    }
}
