//! Dependency hygiene (PM601-PM604): imports of undeclared, private or transitive-only
//! third-party code, and declared dependencies nobody imports. Run by `pymapper deps --check`.
//!
//! ```text
//!   first-party file f ──> unit (nearest pyproject.toml) ──> effective unit (declares deps)
//!     each absolute import row ──> provider
//!        module resolved to a loaded node:  first-party (workspace member) | stdlib | third-party group
//!        not loaded:                        stdlib | ThirdParty(dist, by top_level.txt / RECORD) | external
//!        third-party group without dist, external: built-in name table, else a guess (names.rs)
//!     ──> PM603 private component (`pkg._internal`, `from pkg import _helper`)
//!     ──> declared in the effective unit (any group; for test code also a development group
//!         of the uv workspace root above it)?  no ──> PM604 if a declared dependency requires it
//!         (Requires-Dist closure, read lazily), else PM601; once per import statement and
//!         distribution, at the module token
//!     ──> mark (unit, distribution) used
//!   each unit's [project] dependencies not marked used ──> PM602 (located in its pyproject.toml)
//! ```
//!
//! Confidence follows the mapping ([`Mapping::confidence`]): 100 through installed metadata,
//! 90 through the name table, 60 for a guess; an import guarded by `except ImportError` is an
//! optional dependency and gets at most 50. A declared dependency that installs no importable
//! name is a meta-package: what it requires directly counts as declared, and it is used when
//! one of those is; otherwise (a command-line tool) it is reported as unused with confidence 50.
//!
//! Cost: one pass over the import rows of first-party files with O(1) work per row (hash
//! lookups; nothing is allocated or interned except for findings), then one pass over the
//! declared dependencies. PM604 reads the `METADATA` of the distributions reachable from the
//! declared ones, once each, only when some import is undeclared.

use std::fmt;
use std::path::Path;

use super::declared::{DeclStatus, Declarations, DepKind, display_path};
use super::names::{self, Known, Mapping};
use super::requires::RequiresCache;
use crate::analysis::testfiles::is_test_file;
use crate::extract::facts::import_flags;
use crate::index::{FileId as IxFile, Index, NONE, NodeId};
use crate::intern::{FxHashMap, FxHashSet, Interner, Sym};
use crate::output::{Context, IndexContext};
use crate::resolve::{ImportStatus, Imports};
use crate::roots::{DistId, Distributions, Origin, ProjectUnits};
use crate::rules::codes::{PRIVATE_IMPORT, TRANSITIVE_DEPENDENCY, UNDECLARED_DEPENDENCY, UNUSED_DEPENDENCY};
use crate::rules::{Arg, ArgResolver, FileId, Findings, Location, RuleEngine, SymbolId};

/// Confidence of an import guarded by `except ImportError` (an optional dependency).
const OPTIONAL_IMPORT_CONFIDENCE: u8 = 50;
/// Confidence of PM602 for a distribution that installs no importable name.
const NO_IMPORT_NAME_CONFIDENCE: u8 = 50;

/// What the analysis reads.
pub struct HygieneCtx<'a> {
    pub ix: &'a Index,
    pub imp: &'a Imports,
    pub dists: &'a Distributions,
    pub units: &'a ProjectUnits,
    pub decls: &'a Declarations,
}

/// Which rules to report.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HygieneRules {
    pub undeclared: bool,
    pub unused: bool,
    pub private: bool,
    pub transitive: bool,
}

impl HygieneRules {
    /// The enabled PM6xx rules of `engine`.
    pub fn from_engine(engine: &RuleEngine) -> HygieneRules {
        HygieneRules {
            undeclared: engine.is_enabled(UNDECLARED_DEPENDENCY),
            unused: engine.is_enabled(UNUSED_DEPENDENCY),
            private: engine.is_enabled(PRIVATE_IMPORT),
            transitive: engine.is_enabled(TRANSITIVE_DEPENDENCY),
        }
    }

    /// Every rule.
    pub const ALL: HygieneRules = HygieneRules { undeclared: true, unused: true, private: true, transitive: true };
}

/// Counters of a run.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HygieneStats {
    /// First-party files whose imports were examined.
    pub files: u32,
    /// Their absolute import rows of third-party code.
    pub third_party_rows: u32,
    /// First-party files of units with no dependency declarations (PM601, PM602 and PM604
    /// cannot be decided for them).
    pub undeclared_files: u32,
    /// `METADATA` files read for PM604.
    pub metadata_reads: u32,
}

/// Who provides an imported module.
#[derive(Clone, Copy, Debug)]
enum Provider {
    /// First-party code, in this file (for workspace members imported by their package).
    FirstParty(IxFile),
    /// Third-party code.
    Third(Third),
}

#[derive(Clone, Copy, Debug)]
struct Third {
    /// Normalised distribution name (hygiene interner).
    norm: Sym,
    /// Display name (hygiene interner).
    display: Sym,
    mapping: Mapping,
    /// The import is known to be third-party code (loaded in a third-party group, or attributed
    /// to an installed distribution), not just guessed from an unknown name.
    known: bool,
    /// No other distribution may provide the module: the loaded group, the `RECORD` listing
    /// the module, or the only provider of the top-level name. Otherwise (a namespace package
    /// whose module no `RECORD` lists) any provider of the top-level name counts.
    exact: bool,
    /// Every distribution the name table lists for the top-level name when there are several
    /// (`cv2`: `opencv-python`, `opencv-python-headless`, ...): declaring any one of them
    /// declares the import. Empty otherwise.
    alts: &'static [(&'static str, &'static str)],
}

/// Interned names per distribution, filled on first use.
struct DistNames {
    norm: Vec<Option<(Sym, Sym)>>,
}

impl DistNames {
    fn get(&mut self, dists: &Distributions, d: DistId, names: &mut Interner) -> (Sym, Sym) {
        if let Some(p) = self.norm[d.index()] {
            return p;
        }
        let p = (names.intern(dists.normalized_name(d)), names.intern(dists.name(d)));
        self.norm[d.index()] = Some(p);
        p
    }
}

/// Group of node `v`: its file's, else the group whose root is its top ancestor.
fn group_of(ix: &Index, group_roots: &FxHashMap<NodeId, u32>, v: NodeId) -> u32 {
    match ix.file(v) {
        Some(f) => ix.file_group(f),
        None => group_roots.get(&ix.ancestor_at(v, 0)).copied().unwrap_or(NONE),
    }
}

/// Whether an import path component is private: a leading underscore, but not a dunder
/// (`__version__`) and not the lone `_` (the gettext idiom `from pkg import _`).
#[inline]
pub fn is_private(name: &[u8]) -> bool {
    name.len() > 1 && name[0] == b'_' && !(name.len() > 4 && name.starts_with(b"__") && name.ends_with(b"__"))
}

/// Runs the enabled hygiene rules over every first-party file and every unit, appending the
/// findings to `out` (unfiltered: selection, suppressions and the confidence gate are the
/// caller's). Names in the findings' `Name` arguments are interned in `names` (the interner
/// the declarations were loaded with); render them with [`HygieneContext`].
pub fn analyse(cx: &HygieneCtx<'_>, rules: HygieneRules, names: &mut Interner, out: &mut Findings) -> HygieneStats {
    let mut a = Analysis::new(cx, rules);
    a.meta_packages(names);
    a.imports(names, out);
    if rules.unused {
        a.unused(names, out);
    }
    a.stats.metadata_reads = a.requires.reads();
    a.stats
}

struct Analysis<'c, 'a> {
    cx: &'c HygieneCtx<'a>,
    rules: HygieneRules,
    group_roots: FxHashMap<NodeId, u32>,
    dist_names: DistNames,
    /// `(effective unit, normalised distribution)` imported by some file of the unit.
    used: FxHashSet<(u32, Sym)>,
    /// `(effective unit, top-level import name as an index string)` of third-party imports,
    /// for declared dependencies that are not installed.
    used_top: FxHashSet<(u32, Sym)>,
    /// First-party files per effective unit.
    unit_files: Vec<u32>,
    /// Normalised `[project]` names of every unit: imports of them are never undeclared.
    projects: FxHashSet<Sym>,
    /// Per unit, once computed: distributions required (transitively) by its declared
    /// dependencies -> the declared dependency (display name) that pulls each in.
    closures: FxHashMap<u32, FxHashMap<Sym, Sym>>,
    /// `(unit, distribution)` -> the declared meta-package of the unit that requires it
    /// directly (see [`Analysis::meta_packages`]).
    meta: FxHashMap<(u32, Sym), Sym>,
    requires: RequiresCache,
    stmt: Stmt,
    scratch: Vec<u8>,
    stats: HygieneStats,
}

/// What the import statement being scanned has reported.
#[derive(Default)]
struct Stmt {
    /// Position of its module token.
    pos: Option<(u32, u32)>,
    /// Distributions reported as PM601/PM604 (a namespace package's names may map to several).
    dists: Vec<Sym>,
    /// A private module component reported as PM603.
    private_module: bool,
}

impl<'c, 'a> Analysis<'c, 'a> {
    fn new(cx: &'c HygieneCtx<'a>, rules: HygieneRules) -> Analysis<'c, 'a> {
        let ix = cx.ix;
        let mut group_roots = FxHashMap::default();
        for g in 0..ix.groups().len() as u32 {
            group_roots.insert(ix.group_node(g), g);
        }
        let projects = (0..cx.decls.len()).filter_map(|u| cx.decls.unit(u).project).collect();
        Analysis {
            cx,
            rules,
            group_roots,
            dist_names: DistNames { norm: vec![None; cx.dists.len()] },
            used: FxHashSet::default(),
            used_top: FxHashSet::default(),
            unit_files: vec![0; cx.decls.len() as usize],
            projects,
            closures: FxHashMap::default(),
            meta: FxHashMap::default(),
            requires: RequiresCache::default(),
            stmt: Stmt::default(),
            scratch: Vec::new(),
            stats: HygieneStats::default(),
        }
    }

    /// The unit of every first-party file (`NONE` for other files), in one pass: consecutive
    /// files of one directory share the lookup. 4 bytes per file.
    fn file_units(&self) -> Vec<u32> {
        let ix = self.cx.ix;
        let mut out = vec![NONE; ix.file_count() as usize];
        let mut last_dir: &[u8] = &[];
        let mut last_unit = NONE;
        for f in 0..ix.file_count() {
            if ix.file_origin(f) != Origin::FirstParty {
                continue;
            }
            let dir = ix.file_path(f).parent().map_or(&[][..], |p| p.as_os_str().as_encoded_bytes());
            if last_unit == NONE || dir != last_dir {
                last_dir = dir;
                last_unit = self.cx.units.unit_of_dir(dir);
            }
            out[f as usize] = last_unit;
        }
        out
    }

    fn imports(&mut self, names: &mut Interner, out: &mut Findings) {
        let ix = self.cx.ix;
        let st = &ix.facts().imports;
        let file_units = self.file_units();
        for f in 0..ix.file_count() {
            let unit = file_units[f as usize];
            if unit == NONE {
                continue;
            }
            let eu = self.cx.decls.effective(unit);
            self.stats.files += 1;
            if eu == NONE {
                self.stats.undeclared_files += 1;
            } else {
                self.unit_files[eu as usize] += 1;
            }
            let view = ix.file_view(f);
            // Test code sees the workspace root's development groups (a few byte compares).
            let test = eu != NONE && self.cx.decls.workspace(eu) != NONE && is_test_file(ix, f);
            // Rows of one statement are consecutive and share the module token's position:
            // `from m import (a,\n b)` reports PM601/PM604 (and a private module) once per
            // distribution, at the module token, so a suppression on the statement's first
            // line covers it. `self.stmt` holds what the current statement reported.
            self.stmt = Stmt::default();
            for row in ix.file_facts(f).imports.range() {
                if st.level[row] != 0 {
                    continue;
                }
                let chain = view.import_module(row);
                let Some(&top) = chain.first() else { continue };
                let from = st.flags[row] & (import_flags::FROM | import_flags::STAR) == import_flags::FROM;
                let name = if from { st.name[row] } else { None };
                let Some(p) = self.provider(row, chain, name, names) else { continue };
                let third = match p {
                    Provider::FirstParty(target) => {
                        self.first_party_use(eu, file_units[target as usize]);
                        continue;
                    }
                    Provider::Third(t) => t,
                };
                self.stats.third_party_rows += 1;
                let (mline, mcol) = view.import_module_pos(row);
                if self.stmt.pos != Some((mline, mcol)) {
                    self.stmt.pos = Some((mline, mcol));
                    self.stmt.dists.clear();
                    self.stmt.private_module = false;
                }
                let loc = Location::new(FileId(f), mline, mcol + 1);
                if self.rules.private && third.known {
                    let name_loc = Location::new(FileId(f), st.line[row], st.col[row] + 1);
                    self.private(row, chain, third, loc, name_loc, names, out);
                }
                if eu == NONE {
                    continue;
                }
                let providers = if third.exact { &[][..] } else { self.cx.dists.providers(ix.str(top)) };
                self.used.insert((eu, third.norm));
                self.used_top.insert((eu, top));
                let mut declared = self.declared(eu, third.norm, test);
                for &d in providers {
                    let (n, _) = self.dist_names.get(self.cx.dists, d, names);
                    self.used.insert((eu, n));
                    declared |= self.declared(eu, n, test);
                }
                for &(_, alt) in third.alts {
                    crate::roots::normalize_name(alt.as_bytes(), &mut self.scratch);
                    let n = names.intern(&self.scratch);
                    self.used.insert((eu, n));
                    declared |= self.declared(eu, n, test);
                }
                let decl = self.cx.decls.unit(eu);
                if declared || decl.status != DeclStatus::Declared || decl.dynamic_optional {
                    continue;
                }
                if !(self.rules.undeclared || self.rules.transitive) || self.stmt.dists.contains(&third.norm) {
                    continue;
                }
                self.stmt.dists.push(third.norm);
                let mut conf = third.mapping.confidence();
                if st.flags[row] & import_flags::TRY_IMPORT_ERROR != 0 {
                    conf = conf.min(OPTIONAL_IMPORT_CONFIDENCE);
                }
                let via = if self.rules.transitive { self.required_by(eu, third.norm, names) } else { None };
                let module = self.dotted(chain, chain.len(), None, names);
                match via {
                    Some(via) => out.push(
                        TRANSITIVE_DEPENDENCY,
                        loc,
                        SymbolId::NONE,
                        conf,
                        &[Arg::Name(module.to_raw()), Arg::Name(third.display.to_raw()), Arg::Name(via.to_raw())],
                    ),
                    None if self.rules.undeclared => out.push(
                        UNDECLARED_DEPENDENCY,
                        loc,
                        SymbolId::NONE,
                        conf,
                        &[Arg::Name(module.to_raw()), Arg::Name(third.display.to_raw())],
                    ),
                    None => {}
                }
            }
        }
    }

    /// Whether unit `u` declares `norm` or is (or contains) the project named `norm`. For test
    /// code (`test`), a development group of the uv workspace root above `u` also declares it:
    /// test tools are usually declared once there, while production code of a member must
    /// declare what it imports itself.
    #[inline]
    fn declared(&self, u: u32, norm: Sym, test: bool) -> bool {
        let decls = self.cx.decls;
        decls.find(u, norm).is_some()
            || self.projects.contains(&norm)
            || self.meta.contains_key(&(u, norm))
            || (test && decls.workspace(u) != NONE && decls.declares_group(decls.workspace(u), norm))
    }

    /// Declared dependencies that are installed but provide no importable name are
    /// meta-packages (`pydantic-ai` installs nothing but requires `pydantic-ai-slim`, which
    /// provides `pydantic_ai`): what they require directly (extras honoured) counts as
    /// declared, and their use makes the meta-package used. Editable installs and project
    /// units are not meta-packages. Reads the `METADATA` of those distributions only.
    fn meta_packages(&mut self, names: &mut Interner) {
        let (dists, decls) = (self.cx.dists, self.cx.decls);
        for u in 0..decls.len() {
            if decls.unit(u).status != DeclStatus::Declared {
                continue;
            }
            for dep in decls.deps(u) {
                let Some(d) = dists.by_name(names.resolve(dep.norm)) else { continue };
                // Editable installs (workspace members, often) list no top-level names either.
                if dists.top_level(d).len() > 0 || dists.is_editable(d) || self.projects.contains(&dep.norm) {
                    continue;
                }
                let extras = decls.extras(dep);
                for r in self.requires.get(dists, d, names) {
                    if r.when_extra.is_empty() || r.when_extra.iter().any(|e| extras.contains(e)) {
                        self.meta.entry((u, r.norm)).or_insert(dep.norm);
                    }
                }
            }
        }
    }

    /// Records that a file of effective unit `eu` imports a first-party file of unit
    /// `target_unit`: when that unit's declarations belong to another unit with a `[project]`
    /// name (a workspace member), the member counts as a used dependency of `eu`.
    fn first_party_use(&mut self, eu: u32, target_unit: u32) {
        if eu == NONE || target_unit == NONE {
            return;
        }
        let tu = self.cx.decls.effective(target_unit);
        if tu != NONE
            && tu != eu
            && let Some(p) = self.cx.decls.unit(tu).project
        {
            self.used.insert((eu, p));
        }
    }

    /// Who provides the module of absolute import `row` (dotted module `chain` and, for
    /// `from m import name`, `name`; index strings); `None` for the stdlib and for imports that
    /// resolve nowhere useful.
    fn provider(&mut self, row: usize, chain: &[Sym], name: Option<Sym>, names: &mut Interner) -> Option<Provider> {
        let (ix, imp) = (self.cx.ix, self.cx.imp);
        if let Some(m) = imp.module(row) {
            // A namespace package has no node of its own: use what the import reaches in it.
            let mods = imp.modules();
            let v = mods.node(m).or_else(|| mods.stub(m)).or_else(|| imp.target(row)).or_else(|| {
                let v = imp.module_node(row);
                (v != NONE).then_some(v)
            })?;
            let g = group_of(ix, &self.group_roots, v);
            if g == NONE {
                return None;
            }
            let grp = &ix.groups()[g as usize];
            return match (grp.origin, grp.dist, ix.file(v)) {
                // A first-party namespace package alone (`import ns`) maps to no file.
                (Origin::FirstParty, _, f) => f.map(Provider::FirstParty),
                (Origin::Stdlib, _, _) => None,
                (Origin::ThirdParty, Some(d), Some(_)) => Some(self.dist(d, true, names)),
                // A namespace package node (filed under whichever distribution came first), or
                // code loaded from site-packages that no RECORD claims.
                (Origin::ThirdParty, _, _) => self.by_name(chain, name, true, names),
            };
        }
        match imp.status(row) {
            ImportStatus::ThirdParty => {
                let d = imp.dist(row)?;
                let (d, exact) = self.refine(d, chain, name);
                Some(self.dist(d, exact, names))
            }
            ImportStatus::External => self.by_name(chain, name, false, names),
            _ => None,
        }
    }

    fn dist(&mut self, d: DistId, exact: bool, names: &mut Interner) -> Provider {
        let (norm, display) = self.dist_names.get(self.cx.dists, d, names);
        Provider::Third(Third { norm, display, mapping: Mapping::Metadata, known: true, exact, alts: &[] })
    }

    /// For a top-level name several distributions provide (namespace packages such as
    /// `google`), the one whose `RECORD` lists the deepest module of `chain` (`chain.name`
    /// first: `from google.cloud import storage`), and whether the answer is exact (see
    /// [`Third::exact`]); else `(d, false)`. Only asked for shared names: a few O(1) lookups.
    fn refine(&mut self, d: DistId, chain: &[Sym], name: Option<Sym>) -> (DistId, bool) {
        let (ix, dists) = (self.cx.ix, self.cx.dists);
        if dists.providers(ix.str(chain[0])).len() < 2 {
            return (d, true);
        }
        let site = dists.site(d);
        let full = chain.len() + usize::from(name.is_some());
        for n in (2..=full).rev() {
            self.scratch.clear();
            for (i, &s) in chain.iter().chain(name.as_ref()).take(n).enumerate() {
                if i > 0 {
                    self.scratch.push(b'/');
                }
                self.scratch.extend_from_slice(ix.str(s));
            }
            let base = self.scratch.len();
            for suffix in [&b"/__init__.py"[..], b".py", b"/__init__.pyi", b".pyi"] {
                self.scratch.truncate(base);
                self.scratch.extend_from_slice(suffix);
                if let Some(owner) = dists.owner(site, &self.scratch) {
                    return (owner, true);
                }
            }
        }
        (d, false)
    }

    /// Provider of a module that no loaded module of a distribution answers for: an installed
    /// distribution listing its top-level name, else the name table or a guess. `known`: the
    /// code is third-party for sure (loaded from site-packages).
    fn by_name(&mut self, chain: &[Sym], name: Option<Sym>, known: bool, names: &mut Interner) -> Option<Provider> {
        let top = self.cx.ix.str(*chain.first()?);
        if crate::resolve::is_builtin_stdlib(top) {
            return None;
        }
        if let Some(&d) = self.cx.dists.providers(top).first() {
            let (d, exact) = self.refine(d, chain, name);
            return Some(self.dist(d, exact, names));
        }
        let (dist, mapping, alts) = match names::distributions_of(top)? {
            Known::Table(all) => (all[0].1.as_bytes(), Mapping::Table, if all.len() > 1 { all } else { &[] }),
            Known::Guess(n) => (n, Mapping::Guess, &[][..]),
        };
        crate::roots::normalize_name(dist, &mut self.scratch);
        let norm = names.intern(&self.scratch);
        let display = names.intern(dist);
        Some(Provider::Third(Third { norm, display, mapping, known, exact: true, alts }))
    }

    /// PM603 for `row` when a component of its path is private: located at the module token
    /// (`module_loc`, once per statement) for a private module, at the name (`name_loc`) for a
    /// private name imported from a public module.
    #[allow(clippy::too_many_arguments)]
    fn private(
        &mut self,
        row: usize,
        chain: &[Sym],
        third: Third,
        module_loc: Location,
        name_loc: Location,
        names: &mut Interner,
        out: &mut Findings,
    ) {
        let ix = self.cx.ix;
        let st = &ix.facts().imports;
        let name = if st.flags[row] & import_flags::STAR == 0 { st.name[row] } else { None };
        let (cut, loc) = match chain.iter().position(|&s| is_private(ix.str(s))) {
            Some(_) if self.stmt.private_module => return,
            Some(i) => {
                self.stmt.private_module = true;
                ((i + 1, None), module_loc)
            }
            None => match name {
                Some(n) if is_private(ix.str(n)) => ((chain.len(), Some(n)), name_loc),
                _ => return,
            },
        };
        let path = self.dotted(chain, cut.0, cut.1, names);
        out.push(PRIVATE_IMPORT, loc, SymbolId::NONE, 100, &[Arg::Name(path.to_raw()), Arg::Name(third.display.to_raw())]);
    }

    /// `chain[..n]` (and `.name`) joined with dots, interned in `names`.
    fn dotted(&mut self, chain: &[Sym], n: usize, name: Option<Sym>, names: &mut Interner) -> Sym {
        let ix = self.cx.ix;
        self.scratch.clear();
        for (i, &s) in chain[..n].iter().chain(name.as_ref()).enumerate() {
            if i > 0 {
                self.scratch.push(b'.');
            }
            self.scratch.extend_from_slice(ix.str(s));
        }
        names.intern(&self.scratch)
    }

    /// The declared dependency (display name) of unit `u` that requires `norm`, directly or
    /// transitively, through installed `Requires-Dist` metadata (extras honoured).
    fn required_by(&mut self, u: u32, norm: Sym, names: &mut Interner) -> Option<Sym> {
        if !self.closures.contains_key(&u) {
            let c = self.closure(u, names);
            self.closures.insert(u, c);
        }
        self.closures[&u].get(&norm).copied()
    }

    /// Breadth-first search over `Requires-Dist` from the installed declared dependencies of
    /// unit `u`. A distribution reached again with new extras is expanded again.
    fn closure(&mut self, u: u32, names: &mut Interner) -> FxHashMap<Sym, Sym> {
        let (dists, decls) = (self.cx.dists, self.cx.decls);
        let mut reached: FxHashMap<Sym, Sym> = FxHashMap::default();
        // Extras requested so far per installed distribution.
        let mut seen: FxHashMap<DistId, Vec<Sym>> = FxHashMap::default();
        let mut queue: Vec<(DistId, Vec<Sym>, Sym)> = Vec::new();
        for dep in decls.deps(u) {
            let Some(d) = dists.by_name(names.resolve(dep.norm)) else { continue };
            queue.push((d, decls.extras(dep).to_vec(), dep.name));
        }
        let mut head = 0;
        // Entries of the current breadth-first level not processed yet.
        let mut level_left = 0;
        let mut level: Vec<DistId> = Vec::new();
        while head < queue.len() {
            // One level at a time: its metadata files are read in parallel.
            if level_left == 0 {
                level.clear();
                level.extend(queue[head..].iter().map(|q| q.0));
                level_left = level.len();
                self.requires.prefetch(dists, &level, names);
            }
            level_left -= 1;
            let (d, via) = (queue[head].0, queue[head].2);
            let extras = std::mem::take(&mut queue[head].1);
            head += 1;
            // Expand on the first visit, and again only when new extras enable more.
            let expand = match seen.get_mut(&d) {
                None => {
                    seen.insert(d, extras);
                    true
                }
                Some(known) => {
                    let before = known.len();
                    for e in extras {
                        if !known.contains(&e) {
                            known.push(e);
                        }
                    }
                    known.len() > before
                }
            };
            if !expand {
                continue;
            }
            let all = &seen[&d];
            for r in self.requires.get(dists, d, names) {
                if !r.when_extra.is_empty() && !r.when_extra.iter().any(|w| all.contains(w)) {
                    continue;
                }
                reached.entry(r.norm).or_insert(via);
                if let Some(rd) = dists.by_name(names.resolve(r.norm)) {
                    let more = match seen.get(&rd) {
                        None => true,
                        Some(k) => r.extras.iter().any(|e| !k.contains(e)),
                    };
                    if more {
                        queue.push((rd, r.extras.clone(), via));
                    }
                }
            }
        }
        reached
    }

    /// PM602: `[project]` (and poetry main) dependencies of each unit that no file of the unit
    /// imports. Units without first-party files, with dynamic dependencies or without
    /// declarations are skipped; a dependency declared twice is reported once.
    fn unused(&mut self, names: &mut Interner, out: &mut Findings) {
        let (ix, dists, decls) = (self.cx.ix, self.cx.dists, self.cx.decls);
        let mut reported: FxHashSet<Sym> = FxHashSet::default();
        // A meta-package is used when what it pulls in is.
        for (&(u, r), &m) in &self.meta {
            if self.used.contains(&(u, r)) {
                self.used.insert((u, m));
            }
        }
        for u in 0..decls.len() {
            let decl = decls.unit(u);
            if decl.status != DeclStatus::Declared || self.unit_files[u as usize] == 0 {
                continue;
            }
            reported.clear();
            for dep in decls.deps(u) {
                if dep.kind != DepKind::Main || self.used.contains(&(u, dep.norm)) || !reported.insert(dep.norm) {
                    continue;
                }
                let conf = match dists.by_name(names.resolve(dep.norm)) {
                    Some(d) if dists.top_level(d).len() == 0 => NO_IMPORT_NAME_CONFIDENCE,
                    Some(_) => Mapping::Metadata.confidence(),
                    None => {
                        // Not installed: look for imports of its probable names.
                        let mut used = false;
                        let mut mapping = Mapping::Guess;
                        let used_top = &self.used_top;
                        names::import_names_of(names.resolve(dep.norm), &mut self.scratch, |n, m| {
                            mapping = mapping.min(m);
                            used |= ix.strings().get(n).is_some_and(|s| used_top.contains(&(u, s)));
                        });
                        if used {
                            continue;
                        }
                        mapping.confidence()
                    }
                };
                let loc = Location::new(decls.file_id(u), dep.line, dep.col);
                out.push(UNUSED_DEPENDENCY, loc, SymbolId::NONE, conf, &[Arg::Name(dep.name.to_raw())]);
            }
        }
    }
}

/// Output names for hygiene findings: `pyproject.toml` files by their [`Declarations`] ids,
/// `Name` arguments from the hygiene interner; everything else from the index.
pub struct HygieneContext<'a> {
    inner: IndexContext<'a>,
    names: &'a Interner,
    decls: &'a Declarations,
    root: &'a Path,
    /// Display base for `pyproject.toml` paths (`None` = the root); see
    /// [`IndexContext::relative_to`].
    base: Option<&'a Path>,
    scratch: std::cell::RefCell<Vec<u8>>,
}

impl<'a> HygieneContext<'a> {
    pub fn new(inner: IndexContext<'a>, names: &'a Interner, decls: &'a Declarations, root: &'a Path) -> HygieneContext<'a> {
        HygieneContext { inner, names, decls, root, base: None, scratch: std::cell::RefCell::new(Vec::new()) }
    }

    /// Shows display paths relative to `base` (see [`IndexContext::relative_to`]).
    pub fn relative_to(mut self, base: &'a Path) -> HygieneContext<'a> {
        self.inner = self.inner.relative_to(base);
        self.base = Some(base);
        self
    }

    fn pyproject(&self, file: FileId) -> Option<&'a [u8]> {
        let u = self.decls.unit_of_file(file)?;
        Some(display_path(&self.decls.unit(u).path, self.root).as_os_str().as_encoded_bytes())
    }

    /// Appends the display path of a `pyproject.toml` file; false if `file` is not one.
    fn push_pyproject(&self, file: FileId, out: &mut Vec<u8>) -> bool {
        let Some(u) = self.decls.unit_of_file(file) else {
            return false;
        };
        let path = &self.decls.unit(u).path;
        match self.base {
            Some(base) => {
                let b = base.as_os_str().as_encoded_bytes();
                crate::output::push_relative(path.as_os_str().as_encoded_bytes(), b.strip_suffix(b"/").unwrap_or(b), out);
            }
            None => out.extend_from_slice(display_path(path, self.root).as_os_str().as_encoded_bytes()),
        }
        true
    }
}

impl Context for HygieneContext<'_> {
    fn write_path(&self, file: FileId, out: &mut Vec<u8>) {
        if !self.push_pyproject(file, out) {
            self.inner.write_path(file, out);
        }
    }

    fn write_root_path(&self, file: FileId, out: &mut Vec<u8>) {
        match self.pyproject(file) {
            Some(p) => out.extend_from_slice(p),
            None => self.inner.write_root_path(file, out),
        }
    }

    fn write_root_dir(&self, out: &mut Vec<u8>) {
        self.inner.write_root_dir(out);
    }

    fn write_symbol_id(&self, sym: SymbolId, out: &mut Vec<u8>) {
        self.inner.write_symbol_id(sym, out);
    }

    fn parent_symbol(&self, sym: SymbolId) -> SymbolId {
        self.inner.parent_symbol(sym)
    }

    fn def_end_line(&self, sym: SymbolId) -> u32 {
        self.inner.def_end_line(sym)
    }
}

impl ArgResolver for HygieneContext<'_> {
    fn write_symbol(&self, id: SymbolId, out: &mut dyn fmt::Write) -> fmt::Result {
        self.inner.write_symbol(id, out)
    }

    fn write_name(&self, id: u32, out: &mut dyn fmt::Write) -> fmt::Result {
        match Sym::from_raw(id) {
            Some(s) if s.index() < self.names.len() => crate::output::write_lossy(self.names.resolve(s), out),
            _ => out.write_str("?"),
        }
    }

    fn write_file(&self, id: FileId, out: &mut dyn fmt::Write) -> fmt::Result {
        let mut buf = self.scratch.borrow_mut();
        buf.clear();
        if self.push_pyproject(id, &mut buf) { crate::output::write_lossy(&buf, out) } else { self.inner.write_file(id, out) }
    }

    fn write_contract(&self, index: u32, out: &mut dyn fmt::Write) -> fmt::Result {
        self.inner.write_contract(index, out)
    }
}

