//! Cross-module dead code analysis (design: Checks / CLI, Dead code (a)-(g), (i); rules
//! PM401-PM405).
//!
//! ```text
//!   Index + Imports + ResolvedCalls + options (mode, entrypoints, presets, scripts)
//!     ├─> reach::roots          RootSet: <module> scopes, __main__, tests, entrypoint
//!     │                         decorators / bases / names, scripts, public API (library mode)
//!     ├─> seeds                 per def: a conditional "child seed" (dunder / protocol member,
//!     │                         unknown decorator, external base, dynamic module, exported,
//!     │                         library public) and same-name sibling rings
//!     ├─> liveness levels       one level 0..=100 per node: widest-path propagation from the
//!     │   (phase 1)             roots over call edges, members, bases, overrides (bucket queue)
//!     ├─> module levels         the same over packages/modules: imports, lifted calls, parents
//!     ├─> name fallback         one scan over the evidence hashes of every file, probing the
//!     │   (evidence)            names of the defs, modules and imports still in doubt
//!     ├─> levels (phase 2)      propagation continued from the evidence seeds
//!     └─> findings              PM401 / PM402 / PM403 per def, PM404 per module, PM405 per
//!                               import; confidence = 100 - level
//! ```
//!
//! # Modes and project units
//!
//! Library vs application mode (design (e)) is decided per file through its project unit
//! ([`ProjectUnits`]: the root and every directory below it with a `pyproject.toml`; a file
//! belongs to its nearest unit). `mode = "auto"` resolves each unit on its own ([`FileModes`]):
//! application when its `pyproject.toml` declares commands (`[project.scripts]` /
//! `gui-scripts`; plugin entry-point groups are roots but libraries declare them too) or one of
//! its files is a *plausible* entry point: a `__main__` module or `__main__` guard outside test
//! paths (`tests`, `test`, `testing`, `test_*`, `conftest.py`), private paths (a segment with a
//! leading `_`) and helper directories (`scripts`, `tools`, `examples`, `docs`, `benchmarks`)
//! ([`crate::reach::plausible_entry`]). Ignored guards are still roots (code under a real guard
//! runs). The deciding evidence is kept per unit ([`crate::reach::ModeReason`],
//! [`FileModes::describe`]; `check --stats` prints one line per unit). A forced mode applies to
//! every unit. Everything
//! that differs between the modes (public-API roots, library-public / API-method / star-export
//! seeds, application-mode exported seeds, the `__init__` re-export rule of PM405) looks at the
//! mode of the file at hand: one unit id per file and one flag per unit, no per-def lookup.
//! Without units ([`DeadCodeOptions::units`] `None`) the whole index is one unit.
//!
//! # Liveness levels
//!
//! Instead of a live/dead bit, every node gets a *level*: how sure the analysis is that the node
//! runs (100 = certainly, 0 = no evidence at all). A finding's confidence is `100 - level`, so
//! `check`'s default gate (confidence >= 80) reports nodes whose level is at most 20.
//!
//! Levels propagate like a widest (bottleneck) path: a node is as live as its best path from a
//! root, a path is as live as its weakest step. Steps and their strengths:
//!
//! | step                                                   | level of the target            |
//! |--------------------------------------------------------|--------------------------------|
//! | root (see [`crate::reach::roots`])                     | 100                            |
//! | call edge, `certain` or `likely` (also decorators)     | level of the caller            |
//! | call edge, `possible` (unique-method fallback)         | min(caller, [`level::POSSIBLE`]) |
//! | class -> dunder / abstract / `@override` method, methods of a `Protocol`, nested class | level of the class |
//! | live method -> its class; live class -> its loaded bases | level of the method / class  |
//! | base method -> override (dynamic dispatch, design (b)) | min(base method, overriding class) |
//! | override -> the base method it overrides               | level of the override          |
//! | def -> same-name siblings (property setters, conditional redefinitions, overloads) | level of the def |
//! | scope -> child with an unknown decorator (may register it) | min(scope, [`level::DECORATED`]) |
//! | class with an unloaded base -> its methods (may override a framework method) | min(class, [`level::EXTERNAL_BASE`]) |
//! | module -> its defs when the module uses dynamic access (`getattr` with a variable name, `globals()`, `vars()`, `locals()`, `__getattr__`, `__dict__`) | min(module, [`level::DYNAMIC`]) |
//! | class -> its methods when a loaded ancestor's module uses dynamic access (name-based dispatch in a base printer / visitor) | min(class, [`level::DYNAMIC`]) |
//! | scope -> defs whose name starts with the constant prefix of a `getattr(x, f"pre_{y}")` / `getattr(x, "pre_" + y)` anywhere (prefixes under 3 bytes: in the same file only) | min(scope, [`level::DYNAMIC`]) |
//! | class -> a method overriding a method of a third-party / stdlib class in some first-party class's MRO (framework hook; mixins included) | level of the class |
//! | class -> a method matching an entrypoint `members` rule (pydantic validators) | level of the class |
//! | application mode: module -> defs listed in `__all__` or re-exported | min(module, [`level::EXPORTED`]) |
//! | library mode: scope -> public defs that are not roots (private modules, outside `__all__`) | min(scope, [`level::LIBRARY_PUBLIC`]) |
//! | library mode: class -> its public methods (instances returned by the API reach callers) | min(class, [`level::API_METHOD`]) |
//! | library mode: module -> public defs a public module re-exports with `from m import *` | level of the module |
//! | name fallback: its full dotted path (`pkg.mod.f`, `pkg.mod:f`, prefixes included) is a string literal of a first-party file, or occurs in a config file | [`level::REFERENCED`] |
//! | name fallback: the def's name occurs in its own file or in a file that imports it or its module | [`level::LINKED`] |
//! | name fallback: its name and its module's dotted path occur in the same config file | [`level::LINKED`] |
//! | name fallback: the name occurs anywhere else (attribute, identifier, string, quoted type, config file) | [`level::EVIDENCE`] |
//!
//! A `getattr(x, "name")` / `hasattr(x, "name")` with a constant name is not dynamic access:
//! the string is name evidence like any other. The extractor classifies the name argument
//! ([`crate::extract::facts::file_flags::DYNAMIC_GETATTR`],
//! [`crate::extract::facts::DECL_GETATTR_PREFIX`]).
//!
//! Each node also records the *origin* of its level ([`DeadCode::origin`]): the seed, evidence
//! or capped step its best path is due to. Findings carry it as machine-readable reasons
//! ([`crate::rules::dead_code::reason`]: `no-references`, `only-used-by-unused-code`,
//! `name-elsewhere`, `possible-call-only`, `module-dynamic-access`, ...), and the message
//! names the strongest one: "is never used" only when nothing refers to the node at all.
//!
//! So, at the default gate, a def is reported when nothing reaches it, or when only a
//! library-mode public name keeps it (confidence 80). A function reached only through a
//! `possible` call, carrying an unknown decorator, or whose name appears elsewhere is reported
//! with confidence 50, 40 or 50 (visible with `--confidence 40`, JSON output, the GUI).
//!
//! Name evidence ([`crate::extract::facts`]: identifiers, attribute names and identifier-like
//! string segments of a file, as 32-bit hashes, without counts) cannot tell a call from a
//! reference. An occurrence of a def's name in file `F` is therefore *explained* (ignored) when
//! `F` holds a call or decorator edge with a target to the def or to a base method it overrides
//! (dynamic dispatch), or a class whose resolved base is the def: since the def is not live,
//! that call comes from dead code and must not keep it alive, so dead call chains are reported
//! in full. Three exceptions keep this from hiding real references:
//!
//! - calls from candidates in the def's own call cycle (recursion) explain nothing: a recursive
//!   closure that is returned (`return walk`) stays referenced. The price: a dead recursive
//!   function looks referenced too (confidence 10);
//! - a file that also has an unresolved call of the name (ambiguous receiver, parameter, ...)
//!   explains nothing for it;
//! - the def's own `__all__` entry is evidence like any other string.
//!
//! A file that both calls a def from dead code and references it from live code without a
//! call (`register(f)`) is the case this misses.
//!
//! # Modules (PM404)
//!
//! Module levels use the same propagation over package/module nodes: roots are modules with a
//! non-`<module>` root kind or containing a non-`<module>` root (tests, `__main__`, scripts,
//! entrypoints, public API), and well-known script files ([`SCRIPT_NAMES`]); steps are imports
//! (any kind, `TYPE_CHECKING` included), calls lifted to modules (`possible` ones at
//! [`level::POSSIBLE`]) and module -> parent package (importing `a.b` runs `a`). A module
//! outside any regular package (top-level, or in a directory without `__init__.py`) may be run
//! as a script: [`level::SCRIPT_LIKE`]. The module's name in another file's evidence (for
//! instance `importlib.import_module("pkg.plugins.x")`) gives [`level::EVIDENCE`].
//!
//! # Imports (PM405)
//!
//! An import is used when its bound name (alias, imported name, or the first segment of a plain
//! `import a.b`) occurs in its file's evidence, when another import resolves through it (a
//! re-export: [`Imports::via`]), or when a file that imports its module mentions the name
//! (`mod.name`). Kept without a finding: star imports, `__future__`, `try`/`except ImportError`
//! fallbacks, names in `__all__`, the `import x as x` idiom and, in library mode, every
//! re-export of a package `__init__`. Confidence starts at 100 and loses: 30 when the file uses
//! dynamic access, 40 when another module star-imports the file (public names), 40 when both
//! the name and the module's name occur in another file that does not import the module
//! (string references such as `mock.patch("pkg.mod.name")`), 30 for an application-mode
//! `__init__` re-export, 20 in an `__init__` file plus 20 more for a submodule import there
//! (often for side effects), 50 under `if TYPE_CHECKING:`, 10 for another conditional import.
//! Quoted type expressions (`"Dict[str, Foo]"`, `cast("Foo | None", x)`) are name evidence, so
//! names used only in them count as used.
//!
//! # What is reported
//!
//! Defs, modules and imports of first-party `.py` files (stubs, shadowed and non-importable
//! files are skipped) with confidence at least [`MIN_EMIT`]. Not reported: `@overload`
//! signatures, defs under `if TYPE_CHECKING:`, dunder methods (live with their class) and
//! module-level dunder functions (`__getattr__`, `__dir__`: always live). Packages
//! (`__init__.py`) never get a PM404 finding. A finding is dropped when an enclosing def or the
//! module has an emitted finding of at least the same confidence (the methods of an unused
//! class, the defs and imports of an unused module): deleting the container deletes them. Only
//! containers whose rule is enabled count, so with PM404 or PM402 turned off their contents are
//! reported. Defs are reported at their name; PM404 at line 1, column 1; PM405 at the imported
//! name. The message says "is never used" (PM404: "is never imported") when nothing refers to
//! the node at all, "is only used by unused code" ("is only imported by unused modules")
//! when a `certain` / `likely` call or decorator edge, an import or a loaded subclass (PM404:
//! an import edge) refers to it from code that is itself not live ("... code that may be
//! unused" when its level is inherited from doubtful code), and otherwise names the evidence
//! its confidence is due to ("has no resolved callers (name appears elsewhere)", ...). Suppressions, baselines and `per-file-ignores` apply afterwards through
//! [`RuleEngine::filter`](crate::rules::RuleEngine::filter) (design (i)), so suppressing a
//! container finding also hides what it contains; entrypoint `names` globs are the whitelist.
//!
//! # Cost class: O(V + E) (`CostClass::Graph`)
//!
//! - Roots: see [`crate::reach::roots`] (O(files + defs + decorators)).
//! - Seeds: one pass over the def rows (decorator chains of decorated defs, the bases of
//!   classes for `Protocol`), the import rows (re-export targets) and two evidence lookups per
//!   file.
//! - Propagation: a bucket queue over the 101 levels. A node is expanded once per level
//!   improvement; levels only rise and take at most a dozen distinct values, so each node and
//!   edge is visited a bounded number of times. Per expansion: its call edges, its children
//!   with a seed, and for classes their loaded bases and overriding methods.
//! - Evidence: candidates (defs still below [`level::LINKED`], modules below
//!   [`level::REFERENCED`], unused imports) are
//!   grouped by name behind one hash map; every hash of every first-party file's evidence is
//!   probed once (O(evidence)), in parallel over files with idempotent atomic updates (results
//!   do not depend on scheduling). Work per hit is bounded by the smaller side of each
//!   intersection (see `evidence`), so common names stay linear. Explanations come from one
//!   parallel pass over call edges, classes, call rows and import rows, plus an SCC over the
//!   call edges between candidates (recursion). Dotted paths of the candidates (one qualified
//!   name built per candidate) are a fourth group kind probed in the same pass; first-party
//!   files that are not analysed (outside packages) are probed for paths only; config files
//!   (few) are probed sequentially afterwards.
//! - Memory: two bytes per node (level and why), a labelled call graph without override edges
//!   (`10` bytes per edge plus `8` per node), the override edges (`8` bytes each), one seed
//!   byte per node, the module graphs, and during the evidence pass 4 bytes per node (candidate
//!   index) plus about 40 bytes per candidate. Nothing is allocated per edge during
//!   propagation; no string is built (names are compared as interned ids and hashes).
//!
//! See "Dead code (S4.3c)" in `docs/benchmarks.md` for measured costs.

mod evidence;
mod imports;
mod modules;
mod prop;
mod seeds;

#[cfg(test)]
mod evidence_tests;
#[cfg(test)]
mod tests;

use crate::config::{EntrypointMatcher, Mode};
use crate::extract::config::ConfigEvidence;
use crate::extract::facts::{DefKind, def_flags, evidence_hash};
use crate::index::{FileId, Index, NONE, NodeId, NodeKind};
use crate::reach::{BitSet, FileModes, RootOptions, RootSet, is_library, root_kind, roots};
use crate::roots::ProjectUnits;
use crate::resolve::{Confidence, Imports, ResolvedCalls};
use crate::rules::codes::{UNUSED_CLASS, UNUSED_FUNCTION, UNUSED_IMPORT, UNUSED_METHOD, UNUSED_MODULE};
use crate::rules::{self, Arg, Findings, Location, Pass, RuleEngine, SymbolId};
use crate::timing;
use evidence::extra;
use modules::ModuleProp;
use prop::{DefGraph, Levels};

pub use modules::SCRIPT_NAMES;

/// Liveness levels of the scoring table (see the module docs). Confidence is `100 - level`.
pub mod level {
    /// Certainly live: a root, or reached from one through sure steps.
    pub const LIVE: u8 = 100;
    /// Its full dotted path (`pkg.mod.func`, `pkg.mod:func`) occurs in a string literal or a
    /// config file: a reference by name, almost certainly used (not reported).
    pub const REFERENCED: u8 = 95;
    /// Name fallback: the name occurs in the def's file or in a file importing it or its module.
    pub const LINKED: u8 = 90;
    /// Application mode: listed in `__all__` or re-exported.
    pub const EXPORTED: u8 = 70;
    /// Carries a decorator that may register it (not a known wrapper, not an entrypoint).
    pub const DECORATED: u8 = 60;
    /// A module outside any regular package: may be run as a script.
    pub const SCRIPT_LIKE: u8 = 60;
    /// Reached through a `possible` call edge (unique-method fallback).
    pub const POSSIBLE: u8 = 50;
    /// Library mode: a public method of a class that is not part of the public API roots; its
    /// instances may still reach callers.
    pub const API_METHOD: u8 = 50;
    /// Method of a class with a base that is not loaded: may override a framework method.
    pub const EXTERNAL_BASE: u8 = 50;
    /// Name fallback: the name occurs in some other file.
    pub const EVIDENCE: u8 = 50;
    /// Defined in a module that uses dynamic access (`getattr`, `globals()`, ...).
    pub const DYNAMIC: u8 = 30;
    /// Library mode: a public name that is not part of the public API roots.
    pub const LIBRARY_PUBLIC: u8 = 20;
    /// No evidence of use.
    pub const DEAD: u8 = 0;
}

/// Findings below this confidence are not emitted (the node is almost certainly live).
pub const MIN_EMIT: u8 = 10;

/// Why a node has its level: the step that last raised it (see the module docs).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Why {
    /// Nothing reaches it.
    Dead = 0,
    /// A root ([`crate::reach::root_kind`]).
    Root = 1,
    /// A `certain` / `likely` call (or decorator) edge.
    Call = 2,
    /// A `possible` call edge.
    PossibleCall = 3,
    /// Live with its class: dunder, abstract, `@override` or `Protocol` method, nested class.
    Member = 4,
    /// A method of the class is live.
    Method = 5,
    /// A live subclass inherits from it.
    Subclass = 6,
    /// Overrides a live base method (or is overridden by a live method).
    Override = 7,
    /// A same-name sibling is live.
    Sibling = 8,
    /// Unknown decorator.
    Decorated = 9,
    /// Method of a class with an unloaded base.
    ExternalBase = 10,
    /// Defined in a module with dynamic access.
    Dynamic = 11,
    /// Application mode: in `__all__` or re-exported.
    Exported = 12,
    /// Library mode: a public name.
    LibraryPublic = 13,
    /// Name evidence in a linked file.
    Linked = 14,
    /// Name evidence elsewhere.
    Evidence = 15,
    /// Modules: imported by a live module.
    Import = 16,
    /// Modules: a live submodule.
    Submodule = 17,
    /// Modules: outside any regular package.
    ScriptLike = 18,
    /// Library mode: a public method of a class (instances may reach callers).
    ApiMethod = 19,
    /// Library mode: re-exported by a star import of a public module.
    StarExport = 20,
    /// Name evidence in its own file.
    LinkedSame = 21,
    /// Its dotted path occurs in a string literal.
    StringRef = 22,
    /// Its dotted path occurs in a config file.
    ConfigRef = 23,
    /// Its name occurs in a config file.
    ConfigName = 24,
    /// Its name and its module's dotted path occur in the same config file.
    ConfigLinked = 25,
    /// Overrides a method of a third-party / stdlib base class (framework hook).
    ExternalOverride = 26,
    /// Its name starts with the constant prefix of a dynamic `getattr`.
    DynamicPrefix = 27,
    /// Method of a class whose loaded ancestor's module uses dynamic access.
    DynamicBase = 28,
}

impl Why {
    const ALL: [Why; 29] = [
        Why::Dead,
        Why::Root,
        Why::Call,
        Why::PossibleCall,
        Why::Member,
        Why::Method,
        Why::Subclass,
        Why::Override,
        Why::Sibling,
        Why::Decorated,
        Why::ExternalBase,
        Why::Dynamic,
        Why::Exported,
        Why::LibraryPublic,
        Why::Linked,
        Why::Evidence,
        Why::Import,
        Why::Submodule,
        Why::ScriptLike,
        Why::ApiMethod,
        Why::StarExport,
        Why::LinkedSame,
        Why::StringRef,
        Why::ConfigRef,
        Why::ConfigName,
        Why::ConfigLinked,
        Why::ExternalOverride,
        Why::DynamicPrefix,
        Why::DynamicBase,
    ];

    #[inline]
    pub fn from_u8(v: u8) -> Why {
        Why::ALL[(v as usize).min(Why::ALL.len() - 1)]
    }

    /// Strength of a child seed of this kind (the cap it puts on the child's level).
    #[inline]
    pub(crate) fn seed_strength(self) -> u8 {
        match self {
            Why::Member | Why::StarExport | Why::ExternalOverride => level::LIVE,
            Why::ApiMethod => level::API_METHOD,
            Why::Exported => level::EXPORTED,
            Why::Decorated => level::DECORATED,
            Why::ExternalBase => level::EXTERNAL_BASE,
            Why::Dynamic | Why::DynamicPrefix | Why::DynamicBase => level::DYNAMIC,
            Why::LibraryPublic => level::LIBRARY_PUBLIC,
            _ => 0,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Why::Dead => "dead",
            Why::Root => "root",
            Why::Call => "call",
            Why::PossibleCall => "possible-call",
            Why::Member => "member",
            Why::Method => "method",
            Why::Subclass => "subclass",
            Why::Override => "override",
            Why::Sibling => "sibling",
            Why::Decorated => "decorated",
            Why::ExternalBase => "external-base",
            Why::Dynamic => "dynamic",
            Why::Exported => "exported",
            Why::LibraryPublic => "library-public",
            Why::Linked => "linked",
            Why::Evidence => "evidence",
            Why::Import => "import",
            Why::Submodule => "submodule",
            Why::ScriptLike => "script-like",
            Why::ApiMethod => "api-method",
            Why::StarExport => "star-export",
            Why::LinkedSame => "linked-same-file",
            Why::StringRef => "string-reference",
            Why::ConfigRef => "config-reference",
            Why::ConfigName => "config-name",
            Why::ConfigLinked => "config-linked",
            Why::ExternalOverride => "external-override",
            Why::DynamicPrefix => "dynamic-prefix",
            Why::DynamicBase => "dynamic-base",
        }
    }
}

/// Options of the dead-code analysis.
#[derive(Clone, Copy, Debug)]
pub struct DeadCodeOptions<'a> {
    /// Library vs application mode (`Auto` resolves per unit of `units`, or with
    /// [`is_library`] for the whole index without units).
    pub mode: Mode,
    /// Project units ([`ProjectUnits`]): `mode = "auto"` is resolved per unit. `None`: one
    /// unit, with `scripts` as its script targets.
    pub units: Option<&'a ProjectUnits>,
    /// Name-reference fallback through evidence hashes (`analysis.name-fallback`).
    pub name_fallback: bool,
    /// Entrypoint rules, presets included ([`crate::config::Config::entrypoint_matcher`]).
    pub entrypoints: Option<&'a EntrypointMatcher>,
    /// `[project.scripts]` / `entry_points.txt` targets (see [`RootOptions::scripts`]).
    pub scripts: &'a [&'a [u8]],
    /// Config-file evidence (`analysis.config-evidence`; see [`crate::extract::config`]).
    pub config: Option<&'a ConfigEvidence>,
}

impl Default for DeadCodeOptions<'_> {
    fn default() -> Self {
        DeadCodeOptions { mode: Mode::Auto, units: None, name_fallback: true, entrypoints: None, scripts: &[], config: None }
    }
}

impl<'a> DeadCodeOptions<'a> {
    /// Options of `config` with its compiled entrypoint matcher, the project's units and the
    /// script targets of every unit (dropped when `entrypoints.project-scripts` is off).
    pub fn from_config(
        config: &crate::config::Config,
        entrypoints: &'a EntrypointMatcher,
        units: Option<&'a ProjectUnits>,
        scripts: &'a [&'a [u8]],
    ) -> DeadCodeOptions<'a> {
        let scripts = if config.entrypoints.project_scripts { scripts } else { &[] };
        DeadCodeOptions {
            mode: config.mode,
            units,
            name_fallback: config.analysis.name_fallback,
            entrypoints: Some(entrypoints),
            scripts,
            config: None,
        }
    }

    /// These options with config-file evidence (dropped when `analysis.config-evidence` is
    /// off or the name fallback is).
    pub fn with_config_evidence(self, config: Option<&'a ConfigEvidence>) -> DeadCodeOptions<'a> {
        DeadCodeOptions { config, ..self }
    }
}

/// Counters of one run.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DeadCodeStats {
    /// Library mode was used for every unit.
    pub library: bool,
    /// Project units with analysed files, and how many of them are in library mode.
    pub units: u32,
    pub library_units: u32,
    pub roots: u64,
    /// Nodes at level 100.
    pub live: u64,
    /// Def / module / import candidates probed by the name fallback.
    pub candidates: u64,
    /// Evidence hashes probed.
    pub probes: u64,
    /// Node expansions of the propagation (defs and modules).
    pub expansions: u64,
    /// Findings per rule PM401..PM405 (before filtering).
    pub findings: [u64; 5],
    /// Heap bytes of the def graph (call and override graphs, seeds) at its peak.
    pub graph_bytes: u64,
}

/// Result of the dead-code analysis: a level per node and per module.
#[derive(Clone, Debug, Default)]
pub struct DeadCode {
    level: Vec<u8>,
    why: Vec<u8>,
    /// Per node: the [`Why`] its level is due to (see [`DeadCode::origin`]).
    origin: Vec<u8>,
    modules: modules::ModuleLevels,
    roots: RootSet,
    modes: FileModes,
    /// Per unit: [`unit_flags`].
    unit_flags: Vec<u8>,
    stats: DeadCodeStats,
}

/// What a unit contains ([`DeadCode::unit_flags`]).
pub mod unit_flags {
    /// At least one analysed file.
    pub const FILES: u8 = 1;
    /// An entry point: a root other than a `<module>` scope or the public API (`__main__` guard
    /// or module, test, entrypoint rule match, script target).
    pub const ENTRY: u8 = 2;
}

impl DeadCode {
    /// Liveness level of node `v` (0..=100, see the module docs).
    #[inline]
    pub fn level(&self, v: NodeId) -> u8 {
        self.level[v as usize]
    }

    /// Confidence that `v` is dead (`100 - level`).
    #[inline]
    pub fn confidence(&self, v: NodeId) -> u8 {
        level::LIVE - self.level[v as usize]
    }

    /// The step that gave `v` its level.
    #[inline]
    pub fn why(&self, v: NodeId) -> Why {
        Why::from_u8(self.why[v as usize])
    }

    /// What `v`'s level is due to: the seed, evidence or weakest step that capped its best
    /// path from a root (equal to [`DeadCode::why`] unless the level was inherited through
    /// calls, members, overrides, ...).
    #[inline]
    pub fn origin(&self, v: NodeId) -> Why {
        Why::from_u8(self.origin[v as usize])
    }

    /// Module level of package/module node `v` (`None` for other nodes).
    pub fn module_level(&self, v: NodeId) -> Option<u8> {
        self.modules.level(v)
    }

    /// Why module `v` has its module level.
    pub fn module_why(&self, v: NodeId) -> Option<Why> {
        self.modules.why(v)
    }

    /// The roots the levels started from.
    pub fn roots(&self) -> &RootSet {
        &self.roots
    }

    /// The mode of each file and unit.
    pub fn modes(&self) -> &FileModes {
        &self.modes
    }

    /// [`unit_flags`] of unit `u`.
    pub fn unit_flags(&self, u: u32) -> u8 {
        self.unit_flags.get(u as usize).copied().unwrap_or(0)
    }

    /// Application-mode units with analysed files but no entry point: every definition in them
    /// looks unused.
    pub fn units_without_entry_points(&self) -> impl Iterator<Item = u32> + '_ {
        (0..self.modes.units()).filter(|&u| !self.modes.unit_is_library(u) && self.unit_flags(u) == unit_flags::FILES)
    }

    pub fn stats(&self) -> &DeadCodeStats {
        &self.stats
    }

    /// Heap bytes (capacity).
    pub fn heap_bytes(&self) -> usize {
        self.level.capacity()
            + self.why.capacity()
            + self.origin.capacity()
            + self.modules.heap_bytes()
            + self.roots.heap_bytes()
            + self.modes.heap_bytes()
            + self.unit_flags.capacity()
    }
}

/// Whether file `f` is analysed: first-party, an importable runtime `.py` file.
pub fn analysed_file(ix: &Index, f: FileId) -> bool {
    ix.file_node(f) != NONE && crate::analysis::cycles::runtime_file(ix, f, true)
}

/// Def `v` (row `row` of file `f`) can be reported: not `@overload`, not under
/// `if TYPE_CHECKING:`, not a dunder.
#[inline]
fn reportable_def(ix: &Index, row: usize) -> bool {
    let flags = ix.facts().defs.flags[row];
    flags & (def_flags::OVERLOAD | def_flags::TYPE_CHECKING | def_flags::DUNDER) == 0
}

/// Runs the dead-code analysis (see the module docs) and appends the findings of the enabled
/// PM4xx rules to `out`. Does nothing (an empty result) unless the dead-code pass should run.
pub fn analyse(
    ix: &Index,
    imp: &Imports,
    calls: &ResolvedCalls,
    opts: &DeadCodeOptions<'_>,
    engine: &RuleEngine,
    out: &mut Findings,
) -> DeadCode {
    if !engine.should_run(Pass::DeadCode) {
        return DeadCode::default();
    }
    let emit = Emit {
        function: engine.is_enabled(UNUSED_FUNCTION),
        class: engine.is_enabled(UNUSED_CLASS),
        method: engine.is_enabled(UNUSED_METHOD),
        module: engine.is_enabled(UNUSED_MODULE),
        import: engine.is_enabled(UNUSED_IMPORT),
    };
    run(ix, imp, calls, opts, emit, out)
}

/// Which rules emit.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Emit {
    pub function: bool,
    pub class: bool,
    pub method: bool,
    pub module: bool,
    pub import: bool,
}

impl Emit {
    #[cfg(test)]
    pub(crate) const ALL: Emit = Emit { function: true, class: true, method: true, module: true, import: true };
}

/// [`analyse`] with an explicit rule mask.
pub(crate) fn run(
    ix: &Index,
    imp: &Imports,
    calls: &ResolvedCalls,
    opts: &DeadCodeOptions<'_>,
    emit: Emit,
    out: &mut Findings,
) -> DeadCode {
    let use_scripts = !opts.scripts.is_empty();
    let modes = match opts.units {
        Some(u) => FileModes::resolve(ix, opts.mode, u, use_scripts),
        None if opts.mode == Mode::Auto => FileModes::auto_single(ix, use_scripts),
        None => FileModes::uniform(is_library(ix, opts.mode, use_scripts), true),
    };
    let library_units = modes.library_units();
    let base = if library_units > 0 { RootOptions::library() } else { RootOptions::application() };
    let ropts = RootOptions { entrypoints: opts.entrypoints, scripts: opts.scripts, modes: Some(&modes), ..base };
    let roots = timing::time("deadcode.roots", || roots(ix, Some(imp), Some(calls), &ropts));
    let (g, info) = timing::time("deadcode.graph", || {
        let mut g = DefGraph::new(ix, calls);
        let info = seeds::fill(ix, imp, &mut g, &modes, opts.entrypoints, |f| analysed_file(ix, f));
        (g, info)
    });
    let mut lv = Levels::new(ix.node_count());
    let mut mp = timing::time("deadcode.propagate", || {
        for v in roots.iter() {
            lv.raise(v, level::LIVE, Why::Root);
        }
        lv.run(&g);
        ModuleProp::new(ix, imp, calls, &roots, &info.included)
    });
    let uflags = unit_flags_of(ix, &roots, &modes, &info.included);
    let with_files = |lib: bool| (0..modes.units()).filter(|&u| uflags[u as usize] != 0 && modes.unit_is_library(u) == lib).count() as u32;
    let (lib_units, app_units) = (with_files(true), with_files(false));
    let mut stats = DeadCodeStats {
        library: library_units == modes.units(),
        units: lib_units + app_units,
        library_units: lib_units,
        roots: u64::from(roots.len()),
        ..DeadCodeStats::default()
    };
    // Without the name fallback only the imports are probed (`module.name` uses by importers).
    let (icands, ihits) = timing::time("deadcode.evidence", || {
        let icands = if emit.import { imports::candidates(ix, imp, &info, &modes) } else { Vec::new() };
        let (defs, mods) = if opts.name_fallback {
            (def_candidates(ix, &info.included, &lv.level), module_candidates(ix, &info.included, &mp))
        } else {
            (Vec::new(), Vec::new())
        };
        let keys: Vec<(u32, FileId)> = icands.iter().map(|c| (evidence_hash(ix.str(c.name)), c.file)).collect();
        {
            let config = if opts.name_fallback { opts.config } else { None };
            let cands = evidence::Candidates { defs: &defs, modules: &mods, imports: &keys, config };
            let hits = evidence::scan(ix, imp, calls, &g.over, &info.included, &cands);
            stats.candidates = (defs.len() + mods.len() + keys.len()) as u64;
            stats.probes = hits.probes;
            for (i, &d) in defs.iter().enumerate() {
                let x = hits.def_extra[i];
                if x & extra::STRING_REF != 0 {
                    lv.raise(d, level::REFERENCED, Why::StringRef);
                }
                if x & extra::CONFIG_REF != 0 {
                    lv.raise(d, level::REFERENCED, Why::ConfigRef);
                }
                match hits.defs[i] {
                    0 => {}
                    level::LINKED if x & extra::SAME_FILE != 0 => lv.raise(d, level::LINKED, Why::LinkedSame),
                    level::LINKED => lv.raise(d, level::LINKED, Why::Linked),
                    l => lv.raise(d, l, Why::Evidence),
                }
                if x & extra::CONFIG_LINKED != 0 {
                    lv.raise(d, level::LINKED, Why::ConfigLinked);
                }
                if x & extra::CONFIG_NAME != 0 {
                    lv.raise(d, level::EVIDENCE, Why::ConfigName);
                }
            }
            for (i, &m) in mods.iter().enumerate() {
                let x = hits.mod_extra[i];
                if x & extra::STRING_REF != 0 {
                    mp.raise(m, level::REFERENCED, Why::StringRef);
                }
                if x & extra::CONFIG_REF != 0 {
                    mp.raise(m, level::REFERENCED, Why::ConfigRef);
                }
                if hits.modules[i] {
                    mp.raise(m, level::EVIDENCE, Why::Evidence);
                }
                if x & extra::CONFIG_NAME != 0 {
                    mp.raise(m, level::EVIDENCE, Why::ConfigName);
                }
            }
            lv.run(&g);
            (icands, hits.imports)
        }
    });
    let (modules, mexp) = mp.finish();
    stats.graph_bytes = g.heap_bytes() as u64;
    let referenced = timing::time("deadcode.referenced", || referenced_defs(ix, imp, &g, &lv.level, &info.included));
    drop(g);
    stats.expansions = lv.expansions + mexp;
    stats.live = lv.live_count();
    let mut dc = DeadCode { level: lv.level, why: lv.why, origin: lv.origin, modules, roots, modes, unit_flags: uflags, stats };
    timing::time("deadcode.findings", || {
        findings(ix, &info, &referenced, &mut dc, emit, out);
        if emit.import {
            import_findings(ix, &icands, &ihits, &mut dc, emit.module, out);
        }
    });
    dc
}

/// [`unit_flags`] per unit: analysed files, and entry-point roots (any root kind but
/// `<module>` and public API) in analysed files. O(files + nodes).
fn unit_flags_of(ix: &Index, roots: &RootSet, modes: &FileModes, included: &[bool]) -> Vec<u8> {
    let mut flags = vec![0u8; modes.units() as usize];
    for f in 0..ix.file_count() {
        if included[f as usize] {
            flags[modes.unit(f) as usize] |= unit_flags::FILES;
        }
    }
    for v in roots.iter() {
        if roots.kind(v) & !(root_kind::MODULE | root_kind::PUBLIC) != 0
            && let Some(f) = ix.file(v)
            && included[f as usize]
        {
            flags[modes.unit(f) as usize] |= unit_flags::ENTRY;
        }
    }
    flags
}

/// Defs below [`level::LIVE`] that something refers to, live or not: a `certain` / `likely`
/// call or decorator edge from another def, an import of an included file, or a loaded
/// subclass. Their findings say "only used by unused code" rather than "never used". O(nodes +
/// call edges + import rows + class bases); one bit per node.
fn referenced_defs(ix: &Index, imp: &Imports, g: &DefGraph<'_>, level: &[u8], included: &[bool]) -> BitSet {
    let n = ix.node_count();
    let mut out = BitSet::new(n);
    let rev = g.calls.reverse();
    for v in 0..n {
        if level[v as usize] == level::LIVE || !ix.kind(v).is_def() {
            continue;
        }
        let labels = rev.neighbour_labels(v);
        let sure = |(i, &w): (usize, &u32)| w != v && labels.get(i).is_none_or(|&l| l <= Confidence::Likely as u8);
        if rev.neighbours(v).iter().enumerate().any(sure) {
            out.insert(v);
        }
    }
    for f in 0..ix.file_count() {
        if !included[f as usize] {
            continue;
        }
        for row in ix.file_facts(f).imports.range() {
            if let Some(t) = imp.target(row)
                && ix.kind(t).is_def()
                && ix.file(t) != Some(f)
            {
                out.insert(t);
            }
        }
    }
    let cl = g.classes;
    for c in 0..cl.len() {
        for &b in cl.bases(c) {
            out.insert(cl.node(b));
        }
    }
    out
}

/// Reportable defs of included files below [`level::LINKED`], ascending.
fn def_candidates(ix: &Index, included: &[bool], level: &[u8]) -> Vec<NodeId> {
    let mut out = Vec::new();
    for f in 0..ix.file_count() {
        if !included[f as usize] {
            continue;
        }
        let ff = ix.file_facts(f);
        let m = ix.file_node(f);
        for d in 1..ff.defs.len {
            let v = m + d;
            if level[v as usize] < level::LINKED && reportable_def(ix, (ff.defs.start + d) as usize) {
                out.push(v);
            }
        }
    }
    out.sort_unstable();
    out
}

/// Modules of included files below [`level::REFERENCED`], ascending (name evidence only matters
/// below [`level::EVIDENCE`], but a string or config reference by dotted path also lifts a
/// script-like module).
fn module_candidates(ix: &Index, included: &[bool], mp: &ModuleProp<'_>) -> Vec<NodeId> {
    let mut out = Vec::new();
    for f in 0..ix.file_count() {
        let m = ix.file_node(f);
        if included[f as usize] && ix.kind(m) == NodeKind::Module && mp.level(m).is_some_and(|l| l < level::REFERENCED) {
            out.push(m);
        }
    }
    out.sort_unstable();
    out
}

/// Confidence of the PM404 finding of file `f`'s module (0 if it has none).
fn module_confidence(ix: &Index, dc: &DeadCode, f: FileId) -> u8 {
    let m = ix.file_node(f);
    if ix.kind(m) != NodeKind::Module {
        return 0;
    }
    match dc.modules.level(m) {
        Some(l) if l < level::LIVE && level::LIVE - l >= MIN_EMIT => level::LIVE - l,
        _ => 0,
    }
}

/// Reason bit and PM401-PM403 phrase of an origin ([`DeadCode::origin`]).
fn origin_reason(o: Why) -> (u32, u32) {
    use crate::rules::dead_code::{reason as r, *};
    match o {
        Why::Evidence => (r::NAME_ELSEWHERE, USE_NAME_ELSEWHERE),
        Why::Linked => (r::NAME_IN_IMPORTER, USE_NAME_IMPORTER),
        Why::LinkedSame => (r::NAME_IN_SAME_FILE, USE_NAME_SAME_FILE),
        Why::PossibleCall => (r::POSSIBLE_CALL_ONLY, USE_POSSIBLE),
        Why::Decorated => (r::UNKNOWN_DECORATOR, USE_DECORATED),
        Why::ExternalBase => (r::EXTERNAL_BASE, USE_EXTERNAL_BASE),
        Why::Dynamic => (r::MODULE_DYNAMIC_ACCESS, USE_DYNAMIC),
        Why::DynamicBase => (r::DYNAMIC_BASE, USE_DYNAMIC),
        Why::DynamicPrefix => (r::GETATTR_PREFIX, USE_DYNAMIC_PREFIX),
        Why::Exported => (r::EXPORTED, USE_EXPORTED),
        Why::LibraryPublic => (r::LIBRARY_PUBLIC_NOT_IN_ALL, USE_LIBRARY_PUBLIC),
        Why::ApiMethod => (r::LIBRARY_API_METHOD, USE_API_METHOD),
        Why::ConfigName => (r::CONFIG_REFERENCE, USE_CONFIG_NAME),
        Why::ConfigLinked | Why::ConfigRef => (r::CONFIG_REFERENCE, USE_CONFIG_LINKED),
        Why::StringRef => (r::STRING_REFERENCE, USE_MAYBE_UNUSED),
        Why::ExternalOverride => (r::EXTERNAL_OVERRIDE, USE_MAYBE_UNUSED),
        Why::ScriptLike => (r::SCRIPT_LIKE, USE_MAYBE_UNUSED),
        _ => (0, USE_NEVER),
    }
}

/// Phrase and reasons of a PM401-PM403 finding for def `v` (`referenced`: see
/// [`referenced_defs`]). The reasons carry every fact; the phrase states the strongest one:
/// "is never used" only when nothing refers to it at all, "is only used by unused code" when
/// something refers to it from code that is not live (or "... code that may be unused" when its
/// level is inherited from doubtful callers), else the evidence its confidence is due to.
fn def_wording(dc: &DeadCode, v: NodeId, referenced: bool) -> (u32, u32) {
    use crate::rules::dead_code::{reason as r, *};
    let used = if referenced { r::ONLY_USED_BY_UNUSED_CODE } else { 0 };
    if dc.level(v) == level::DEAD {
        return if referenced { (USE_ONLY_UNUSED, used) } else { (USE_NEVER, r::NO_REFERENCES) };
    }
    let origin = dc.origin(v);
    let (bit, phrase) = origin_reason(origin);
    let inherited = dc.why(v) != origin;
    let phrase = match (referenced, inherited) {
        (true, true) => USE_MAYBE_UNUSED,
        (true, false) => USE_ONLY_UNUSED,
        (false, _) if bit == 0 => USE_NEVER,
        (false, _) => phrase,
    };
    let bits = bit | used;
    (phrase, if bits == 0 { r::NO_REFERENCES } else { bits })
}

/// Phrase and reasons of the PM404 finding of module `m` (same rules as [`def_wording`], with
/// imports as the references).
fn module_wording(dc: &DeadCode, m: NodeId) -> (u32, u32) {
    use crate::rules::dead_code::{reason as r, *};
    let imported = dc.modules.imported(m);
    let used = if imported { r::ONLY_USED_BY_UNUSED_CODE } else { 0 };
    if dc.modules.level(m).unwrap_or(0) == level::DEAD {
        return if imported { (IMP_ONLY_UNUSED, used) } else { (IMP_NEVER, r::NO_REFERENCES) };
    }
    let origin = dc.modules.origin(m).unwrap_or(Why::Dead);
    let (bit, phrase) = match origin {
        Why::Evidence => (r::NAME_ELSEWHERE, IMP_NAME_ELSEWHERE),
        Why::PossibleCall => (r::POSSIBLE_CALL_ONLY, IMP_POSSIBLE),
        Why::ScriptLike => (r::SCRIPT_LIKE, IMP_SCRIPT_LIKE),
        Why::ConfigName => (r::CONFIG_REFERENCE, IMP_CONFIG_NAME),
        Why::StringRef => (r::STRING_REFERENCE, IMP_MAYBE_UNUSED),
        Why::ConfigRef => (r::CONFIG_REFERENCE, IMP_MAYBE_UNUSED),
        _ => (0, IMP_NEVER),
    };
    let inherited = dc.modules.why(m) != Some(origin);
    let phrase = match (imported, inherited) {
        (true, true) => IMP_MAYBE_UNUSED,
        (true, false) => IMP_ONLY_UNUSED,
        (false, _) => phrase,
    };
    let bits = bit | used;
    (phrase, if bits == 0 { r::NO_REFERENCES } else { bits })
}

/// PM401-PM404 findings, files in FileId order (module first, then defs in source order).
/// `referenced`: see [`referenced_defs`] (the wording of PM401-PM403).
fn findings(ix: &Index, info: &seeds::FileInfo, referenced: &BitSet, dc: &mut DeadCode, emit: Emit, out: &mut Findings) {
    let defs = &ix.facts().defs;
    // Nearest reported ancestor defs: (subtree end, confidence).
    let mut stack: Vec<(NodeId, u8)> = Vec::new();
    for f in 0..ix.file_count() {
        if !info.included[f as usize] {
            continue;
        }
        let file = rules::FileId(f);
        let m = ix.file_node(f);
        // A PM404 finding contains the module's defs and imports only when PM404 is emitted.
        let mconf = if emit.module { module_confidence(ix, dc, f) } else { 0 };
        if mconf > 0 {
            let (phrase, reasons) = module_wording(dc, m);
            let at = Location::new(file, 1, 1);
            out.push_with_reasons(UNUSED_MODULE, at, SymbolId(m), mconf, &[Arg::Symbol(SymbolId(m)), Arg::Phrase(phrase)], reasons);
            dc.stats.findings[3] += 1;
        }
        let ff = ix.file_facts(f);
        stack.clear();
        for d in 1..ff.defs.len {
            let v = m + d;
            let row = (ff.defs.start + d) as usize;
            while stack.last().is_some_and(|&(end, _)| end <= v) {
                stack.pop();
            }
            let l = dc.level[v as usize];
            if l == level::LIVE || level::LIVE - l < MIN_EMIT || !reportable_def(ix, row) {
                continue;
            }
            let conf = level::LIVE - l;
            if mconf.max(stack.last().map_or(0, |&(_, c)| c)) >= conf {
                continue;
            }
            let (rule, on, slot) = match DefKind::from_u8(defs.kind[row]) {
                DefKind::Class => (UNUSED_CLASS, emit.class, 1),
                _ if defs.flags[row] & def_flags::METHOD != 0 => (UNUSED_METHOD, emit.method, 2),
                _ => (UNUSED_FUNCTION, emit.function, 0),
            };
            // Only a finding that is emitted contains the defs below it.
            if on {
                stack.push((ix.subtree(v).end, conf));
                let at = Location::new(file, defs.name_line[row], defs.name_col[row] + 1);
                let (phrase, reasons) = def_wording(dc, v, referenced.contains(v));
                out.push_with_reasons(rule, at, SymbolId(v), conf, &[Arg::Symbol(SymbolId(v)), Arg::Phrase(phrase)], reasons);
                dc.stats.findings[slot] += 1;
            }
        }
    }
}

/// PM405 findings, in candidate (file, row) order.
fn import_findings(
    ix: &Index,
    cands: &[imports::ImportCand],
    hits: &[u8],
    dc: &mut DeadCode,
    module_on: bool,
    out: &mut Findings,
) {
    let st = &ix.facts().imports;
    for (c, &hit) in cands.iter().zip(hits) {
        let Some((conf, reasons)) = imports::confidence(c, hit) else { continue };
        if conf < MIN_EMIT || (module_on && module_confidence(ix, dc, c.file) >= conf) {
            continue;
        }
        let row = c.row as usize;
        let at = Location::new(rules::FileId(c.file), st.line[row], st.col[row] + 1);
        out.push_with_reasons(UNUSED_IMPORT, at, SymbolId(ix.file_node(c.file)), conf, &[Arg::Name(c.name.to_raw())], reasons);
        dc.stats.findings[4] += 1;
    }
}
