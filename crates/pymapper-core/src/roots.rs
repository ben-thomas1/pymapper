//! Import roots: where `import x` is resolved from, and where every file comes from
//! (design: Loading / index).
//!
//! [`discover`] builds the search path of a project without running Python:
//!
//! ```text
//!   RootId  kind           origin        source
//!   0..     Src/Configured first-party   `src` config, else build-backend hints + `src/` layout
//!   ..      Project        first-party   the project root (nearest pyproject.toml)
//!   ..      Stdlib         stdlib        lib/python3.N of the base interpreter (pyvenv.cfg)
//!   ..      SitePackages   third-party   <venv>/lib/python3.N/site-packages (+ system site)
//!   ..      Pth            first/third   path lines of *.pth files (path-style editables)
//!   ..      EditableHook   first/third   packages mapped by editable import hooks (prefix)
//! ```
//!
//! The order is `sys.path` order (first-party first), so [`RootId`] doubles as priority. A
//! `.pth` / hook root inside the project (and outside the venv) is first-party: in a monorepo
//! the workspace members installed in editable mode are first-party code.
//!
//! - Venv: `venv` config, else `$VIRTUAL_ENV`, else `.venv` / `venv` in the project root.
//! - Python version: config, else `pyvenv.cfg`, else `requires-python`, else the stdlib found.
//!   It picks the stdlib (falling back to the venv's interpreter when not installed); the venv's
//!   site-packages always follow the venv's own version (`pyvenv.cfg`, else `lib/python3.N`).
//! - Distributions ([`Distributions`]): every `*.dist-info` / `*.egg-info` of each site is parsed
//!   (always, the toggles only control analysis): name, version, `RECORD` files (third-party
//!   code needs no walk), top-level import names, editable flag. File -> distribution and
//!   import name -> distribution lookups are O(1) hash lookups.
//! - Toggles: [`RootOptions::include_deps`] / [`RootOptions::include_stdlib`] set
//!   [`ImportRoot::analyse`]; [`Roots::sources`] lists what to load.
//! - Files: [`Roots::classify`] maps a path to its root (longest root prefix, O(roots)),
//!   origin and distribution; [`Roots::module_map`] / [`ModuleMapBuilder`] apply PEP 420 / PEP
//!   561 semantics and `.pyi`-over-`.py` preference ([`modules`]).
//!
//! Never fails: problems become [`Diagnostic`]s.

mod dist;
mod modules;
mod project;
mod pth;
mod pyvenv;
mod units;

#[cfg(test)]
mod tests;

pub use dist::{DistId, Distributions, normalize_name};
pub use modules::{FileStatus, ModuleKind, ModuleMap, ModuleMapBuilder, is_identifier};
pub use project::{PyprojectInfo, find_project_root, is_src_layout, parse_pyproject, requires_python_floor};
pub use units::ProjectUnits;
pub use pyvenv::{PyvenvCfg, Venv, VenvSource, find_site_packages, find_stdlib, parse_pyvenv_cfg, parse_version_prefix};

use std::ffi::{OsStr, OsString};
use std::fmt;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use crate::config::{Config, PythonVersion, normalize};
use crate::intern::FxHashMap;
use crate::walk::{PathList, WalkOptions};

/// Index of an import root in [`Roots::roots`]; lower is searched first.
pub type RootId = u32;

/// Where code comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[repr(u8)]
pub enum Origin {
    FirstParty,
    ThirdParty,
    Stdlib,
}

impl Origin {
    pub fn as_str(self) -> &'static str {
        match self {
            Origin::FirstParty => "first-party",
            Origin::ThirdParty => "third-party",
            Origin::Stdlib => "stdlib",
        }
    }
}

impl fmt::Display for Origin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Why a directory is an import root.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum RootKind {
    /// An entry of the `src` option.
    Configured,
    /// A `src/` layout or a build-backend source directory from `pyproject.toml`.
    Src,
    /// The project root.
    Project,
    /// The standard library.
    Stdlib,
    /// A site-packages directory.
    SitePackages,
    /// A path line of a `.pth` file (path-style editable installs, `easy-install.pth`, ...).
    Pth,
    /// A module or package mapped by an editable import hook; see [`ImportRoot::prefix`].
    EditableHook,
}

/// One search-path entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImportRoot {
    /// Absolute, lexically normalised.
    pub path: PathBuf,
    pub kind: RootKind,
    pub origin: Origin,
    /// Dotted module name of `path` for [`RootKind::EditableHook`] roots (files below are
    /// `prefix.rel`; a file root is the module `prefix` itself). Empty otherwise.
    pub prefix: Box<[u8]>,
    /// `path` is a file (single-module hook mapping).
    pub is_file: bool,
    /// The distribution this root belongs to: the owner of the `.pth` that added it, or of the
    /// project (when the project itself is installed in editable mode).
    pub dist: Option<DistId>,
    /// Site index in [`Distributions::sites`] for [`RootKind::SitePackages`].
    pub site: Option<usize>,
    /// Whether files under this root are analysed (the deps / stdlib toggles).
    pub analyse: bool,
}

/// Which problem a [`Diagnostic`] reports.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiagnosticKind {
    /// Configured / `$VIRTUAL_ENV` venv unusable, unreadable `pyvenv.cfg`, deps requested but no
    /// venv found.
    Venv,
    /// The stdlib was requested but not found.
    Stdlib,
    /// Unreadable `pyproject.toml`.
    Pyproject,
    /// Unreadable `.pth` file.
    Pth,
    /// Unreadable `METADATA` / `PKG-INFO`.
    Metadata,
    /// Missing or unreadable `RECORD`.
    Record,
    /// Unlistable site-packages directory.
    ReadDir,
}

/// A non-fatal problem found while discovering roots.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Diagnostic {
    pub kind: DiagnosticKind,
    pub path: PathBuf,
    pub message: String,
}

impl fmt::Display for Diagnostic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.path.display(), self.message)
    }
}

/// Where [`Roots::python_version`] came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VersionSource {
    Config,
    Venv,
    RequiresPython,
    /// The highest `lib/python3.N` found for the interpreter.
    Stdlib,
}

/// Inputs of [`discover`]. Environment values are fields (not read by `discover`) so tests and
/// embedders control them; [`RootOptions::from_config`] fills them from the process.
#[derive(Clone, Debug, Default)]
pub struct RootOptions {
    /// First-party import roots (absolute, or relative to the project root). Empty =
    /// auto-detect.
    pub src: Vec<PathBuf>,
    /// Analyse third-party code.
    pub include_deps: bool,
    /// Analyse the stdlib.
    pub include_stdlib: bool,
    /// Virtualenv to use (absolute, or relative to the project root).
    pub venv: Option<PathBuf>,
    /// Target Python version. Sets [`Roots::python_version`] and picks the stdlib; never the
    /// venv's site-packages, which follow the venv's own interpreter.
    pub python_version: Option<PythonVersion>,
    /// Value of `$VIRTUAL_ENV`.
    pub virtual_env: Option<PathBuf>,
    /// Value of `$PATH` (to find an interpreter when there is no venv).
    pub path_env: Option<OsString>,
}

impl RootOptions {
    /// Options from a loaded configuration plus `$VIRTUAL_ENV` and `$PATH`.
    pub fn from_config(config: &Config) -> RootOptions {
        RootOptions {
            src: config.src.clone(),
            include_deps: config.include_deps,
            include_stdlib: config.include_stdlib,
            venv: config.venv.clone(),
            python_version: config.python_version,
            virtual_env: std::env::var_os("VIRTUAL_ENV").map(PathBuf::from),
            path_env: std::env::var_os("PATH"),
        }
    }
}

/// The discovered search path, environment and distributions.
pub struct Roots {
    /// Absolute, normalised project root.
    pub project_root: PathBuf,
    /// Search path in `sys.path` order.
    pub roots: Vec<ImportRoot>,
    pub venv: Option<Venv>,
    pub python_version: Option<PythonVersion>,
    pub version_source: Option<VersionSource>,
    /// The stdlib directory, if found.
    pub stdlib: Option<PathBuf>,
    pub pyproject: Option<PyprojectInfo>,
    pub dists: Distributions,
    pub include_deps: bool,
    pub include_stdlib: bool,
    pub diagnostics: Vec<Diagnostic>,
    /// Root ids by path length, longest first, for [`Roots::classify`].
    match_order: Vec<RootId>,
}

/// Where a file belongs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Located<'p> {
    pub root: RootId,
    pub origin: Origin,
    /// Path relative to the root (`/`-separated); for a file root, the file name.
    pub rel: &'p [u8],
    /// The root is the file itself (its module name is the root's prefix).
    pub single: bool,
    /// Owning distribution: from `RECORD` under site-packages, else the root's.
    pub dist: Option<DistId>,
}

/// What to load for analysis.
#[derive(Clone, Debug)]
pub enum Source {
    /// Walk `path` with `opts`.
    Walk { path: PathBuf, opts: WalkOptions, origin: Origin },
    /// The `.py` / `.pyi` files a distribution's `RECORD` lists and it owns (no walk needed;
    /// [`Distributions::owned_files`], relative to [`Distributions::site_path`]; see
    /// [`Roots::add_record_files`]). A file listed by several distributions is loaded once.
    Record { dist: DistId },
}

#[inline]
pub(crate) fn bytes_path(b: &[u8]) -> &Path {
    Path::new(OsStr::from_bytes(b))
}

#[inline]
fn path_bytes(p: &Path) -> &[u8] {
    p.as_os_str().as_bytes()
}

/// `hook` (an editable hook root) lies in the first-party root `fp` and maps its files to the
/// same module names: its path relative to `fp` is its dotted prefix (`proj -> <fp>/proj`,
/// `mod -> <fp>/mod.py`).
fn hook_redundant(hook: &ImportRoot, fp: &ImportRoot) -> bool {
    if hook.origin != Origin::FirstParty || !matches!(fp.kind, RootKind::Configured | RootKind::Src | RootKind::Project) {
        return false;
    }
    let Ok(rel) = hook.path.strip_prefix(&fp.path) else { return false };
    let mut rel = path_bytes(rel);
    if hook.is_file {
        let Some(stem) = rel.strip_suffix(b".py").or_else(|| rel.strip_suffix(b".pyi")) else { return false };
        rel = stem;
    }
    rel.len() == hook.prefix.len() && rel.iter().zip(&hook.prefix).all(|(&a, &b)| a == b || (a == b'/' && b == b'.'))
}

/// `path` is `dir` or below it (component-wise).
fn is_under(path: &Path, dir: &Path) -> bool {
    let (p, d) = (path_bytes(path), path_bytes(dir));
    p.starts_with(d) && (p.len() == d.len() || p[d.len()] == b'/' || d.ends_with(b"/"))
}

/// Accumulates roots, deduplicated by path (the first, highest-priority entry wins).
struct Builder {
    roots: Vec<ImportRoot>,
    by_path: FxHashMap<PathBuf, RootId>,
}

impl Builder {
    /// Adds a root unless its path is already one (then only a missing `dist` is filled in).
    fn push(&mut self, root: ImportRoot) {
        if let Some(&id) = self.by_path.get(&root.path) {
            let existing = &mut self.roots[id as usize];
            if existing.dist.is_none() && existing.prefix == root.prefix {
                existing.dist = root.dist;
            }
            return;
        }
        self.by_path.insert(root.path.clone(), self.roots.len() as RootId);
        self.roots.push(root);
    }
}

fn root(path: PathBuf, kind: RootKind, origin: Origin, analyse: bool) -> ImportRoot {
    ImportRoot { path, kind, origin, prefix: Box::default(), is_file: false, dist: None, site: None, analyse }
}

/// Discovers the import roots of the project at `project_root` (absolute). See the module docs.
pub fn discover(project_root: &Path, opts: &RootOptions) -> Roots {
    let mut diags = Vec::new();
    let project_root = normalize(project_root);
    let abs = |p: &Path| normalize(&project_root.join(p));

    let pyproject = match project::read_pyproject(&project_root) {
        Ok(p) => p,
        Err(message) => {
            let path = project_root.join(crate::config::PYPROJECT_FILE);
            diags.push(Diagnostic { kind: DiagnosticKind::Pyproject, path, message });
            None
        }
    };

    let venv_cfg = opts.venv.as_deref().map(abs);
    let virtual_env = opts.virtual_env.as_deref().map(abs);
    let venv = pyvenv::find_venv(&project_root, venv_cfg.as_deref(), virtual_env.as_deref(), &mut diags);

    let mut python_version = opts.python_version;
    let mut version_source = python_version.map(|_| VersionSource::Config);
    if python_version.is_none()
        && let Some(v) = venv.as_ref().and_then(|v| v.version)
    {
        python_version = Some(v);
        version_source = Some(VersionSource::Venv);
    }
    if python_version.is_none()
        && let Some(v) = pyproject.as_ref().and_then(|p| p.requires_python)
    {
        python_version = Some(v);
        version_source = Some(VersionSource::RequiresPython);
    }
    // The stdlib of the target version is preferred. `requires-python` is only a floor: let any
    // interpreter match when the floor's exact version is not installed. A configured version
    // that is not installed falls back to the venv's own interpreter (reported).
    let venv_version = venv.as_ref().and_then(|v| v.version);
    let path_env = opts.path_env.as_deref();
    let mut stdlib = pyvenv::find_stdlib(python_version, venv.as_ref(), path_env);
    if stdlib.is_none() && version_source == Some(VersionSource::RequiresPython) {
        stdlib = pyvenv::find_stdlib(None, venv.as_ref(), path_env);
    }
    if stdlib.is_none()
        && version_source == Some(VersionSource::Config)
        && let (Some(want), Some(have)) = (python_version, venv_version)
        && want != have
    {
        stdlib = pyvenv::find_stdlib(Some(have), venv.as_ref(), path_env);
        if opts.include_stdlib && stdlib.is_some() {
            let message = format!("standard library for Python {want} (`python-version`) not found; using the venv's Python {have} standard library");
            diags.push(Diagnostic { kind: DiagnosticKind::Stdlib, path: project_root.clone(), message });
        }
    }
    if let Some((_, v)) = &stdlib
        && (python_version.is_none() || version_source == Some(VersionSource::RequiresPython))
        && python_version != Some(*v)
    {
        python_version = Some(*v);
        version_source = Some(VersionSource::Stdlib);
    }
    if opts.include_stdlib && stdlib.is_none() {
        let message = match python_version {
            Some(v) => format!("standard library for Python {v} not found (set `venv` or put python{v} on PATH)"),
            None => "standard library not found (set `venv` or `python-version`)".to_owned(),
        };
        diags.push(Diagnostic { kind: DiagnosticKind::Stdlib, path: project_root.clone(), message });
    }
    if opts.include_deps && venv.is_none() {
        let message = "third-party analysis requested but no virtualenv found (set `venv`)".to_owned();
        diags.push(Diagnostic { kind: DiagnosticKind::Venv, path: project_root.clone(), message });
    }

    let mut b = Builder { roots: Vec::new(), by_path: FxHashMap::default() };

    // 1. First-party.
    if !opts.src.is_empty() {
        for s in &opts.src {
            b.push(root(abs(s), RootKind::Configured, Origin::FirstParty, true));
        }
    } else {
        if let Some(info) = &pyproject {
            for d in &info.source_dirs {
                let p = abs(d);
                if p != project_root && p.is_dir() {
                    b.push(root(p, RootKind::Src, Origin::FirstParty, true));
                }
            }
        }
        let src = project_root.join("src");
        if is_src_layout(&src) {
            b.push(root(src, RootKind::Src, Origin::FirstParty, true));
        }
        b.push(root(project_root.clone(), RootKind::Project, Origin::FirstParty, true));
    }

    // 2. Stdlib.
    if let Some((dir, _)) = &stdlib {
        b.push(root(dir.clone(), RootKind::Stdlib, Origin::Stdlib, opts.include_stdlib));
    }

    // 3. Site-packages.
    let mut dists = Distributions::default();
    let mut sites: Vec<PathBuf> = venv.as_ref().map(|v| v.site_packages.clone()).unwrap_or_default();
    if let Some(v) = &venv
        && v.cfg.as_ref().is_some_and(|c| c.include_system_site_packages)
    {
        // The base interpreter's site-packages belongs to the venv's version, which can differ
        // from the target version the stdlib was picked for.
        let sys_stdlib = match &stdlib {
            Some((dir, ver)) if v.version.is_none_or(|vv| vv == *ver) => Some(dir.clone()),
            _ => v.version.and_then(|vv| pyvenv::find_stdlib(Some(vv), Some(v), path_env)).map(|(dir, _)| dir),
        };
        if let Some(dir) = sys_stdlib {
            let sys_site = dir.join("site-packages");
            if sys_site.is_dir() && !sites.contains(&sys_site) {
                sites.push(sys_site);
            }
        }
    }
    let mut site_roots: Vec<(usize, PathBuf)> = Vec::new();
    for sp in sites {
        let sp = normalize(&sp);
        let Some(idx) = dists.add_site(&sp, &mut diags) else { continue };
        let mut r = root(sp.clone(), RootKind::SitePackages, Origin::ThirdParty, opts.include_deps);
        r.site = Some(idx);
        b.push(r);
        site_roots.push((idx, sp));
    }

    // 4. `.pth` files: path entries, then editable hook mappings.
    let venv_prefix = venv.as_ref().map(|v| normalize(&v.prefix));
    let origin_of = |p: &Path| {
        let in_venv = venv_prefix.as_deref().is_some_and(|v| is_under(p, v));
        let in_site = p.components().any(|c| c.as_os_str() == "site-packages");
        if is_under(p, &project_root) && !in_venv && !in_site { Origin::FirstParty } else { Origin::ThirdParty }
    };
    let mut hooks: Vec<ImportRoot> = Vec::new();
    for (idx, sp) in &site_roots {
        let mut items = Vec::new();
        pth::read_site_pth(sp, &mut items, &mut diags);
        for item in items {
            match item {
                pth::PthItem::Path { path, pth } => {
                    let origin = origin_of(&path);
                    let analyse = origin == Origin::FirstParty || opts.include_deps;
                    let mut r = root(path, RootKind::Pth, origin, analyse);
                    r.dist = dists.owner(*idx, &pth);
                    b.push(r);
                }
                pth::PthItem::Mapped { name, path, pth } => {
                    let origin = origin_of(&path);
                    let analyse = origin == Origin::FirstParty || opts.include_deps;
                    let dist = dists.owner(*idx, &pth);
                    if let Some(d) = dist {
                        let top = name.split(|&c| c == b'.').next().unwrap_or(&name);
                        dists.add_top_level(d, top);
                    }
                    let is_file = path.is_file();
                    let mut r = root(path, RootKind::EditableHook, origin, analyse);
                    r.prefix = name;
                    r.is_file = is_file;
                    r.dist = dist;
                    hooks.push(r);
                }
            }
        }
    }
    for r in hooks {
        b.push(r);
    }
    dists.finish();

    // The project itself installed in editable mode: attribute first-party roots to it.
    let mut roots = b.roots;
    let project_dist = pyproject.as_ref().and_then(|p| p.name.as_deref()).and_then(|n| dists.by_name(n.as_bytes()));
    if let Some(d) = project_dist {
        for r in roots.iter_mut().filter(|r| r.origin == Origin::FirstParty && r.dist.is_none()) {
            r.dist = Some(d);
        }
    }

    let match_order = match_order(&roots);
    Roots {
        project_root,
        roots,
        venv,
        python_version,
        version_source,
        stdlib: stdlib.map(|(d, _)| d),
        pyproject,
        dists,
        include_deps: opts.include_deps,
        include_stdlib: opts.include_stdlib,
        diagnostics: diags,
        match_order,
    }
}

/// Root ids in [`Roots::classify`] order: longest prefix first, except that an editable hook
/// root that names its files exactly as an enclosing first-party root does (setuptools' `proj ->
/// <root>/src/proj` mapping) yields to it: the project's own files keep first-party priority
/// whichever way it is installed.
fn match_order(roots: &[ImportRoot]) -> Vec<RootId> {
    let yields = |r: &ImportRoot| r.kind == RootKind::EditableHook && roots.iter().any(|o| hook_redundant(r, o));
    let mut order: Vec<RootId> = (0..roots.len() as RootId).collect();
    order.sort_by_cached_key(|&i| {
        let r = &roots[i as usize];
        (yields(r), std::cmp::Reverse(path_bytes(&r.path).len()), i)
    });
    order
}

impl Roots {
    /// Adds the first-party import roots of the project units found by the walk
    /// ([`ProjectUnits::import_dirs`]), after the existing first-party roots and before the
    /// project root. A unit that an editable install in the venv already maps (a first-party
    /// `.pth` / hook root at or below the unit directory) is left to it: the install is what
    /// Python really imports. Directories that already are roots or lie outside the project
    /// root are skipped. Call before classifying files. O(roots * dirs).
    pub fn add_unit_roots(&mut self, units: &ProjectUnits) {
        let mut added = false;
        let dirs = units.import_dirs();
        for (i, d) in dirs.iter().enumerate() {
            let unit_dir = units.dir(units.import_dir_units()[i]);
            let installed = self.roots.iter().any(|r| {
                r.origin == Origin::FirstParty && matches!(r.kind, RootKind::Pth | RootKind::EditableHook) && is_under(&r.path, unit_dir)
            });
            if installed || !is_under(d, &self.project_root) || self.roots.iter().any(|r| r.path == *d) {
                continue;
            }
            let at = self.roots.iter().position(|r| r.origin != Origin::FirstParty || r.kind == RootKind::Project);
            let at = at.unwrap_or(self.roots.len());
            self.roots.insert(at, root(d.clone(), RootKind::Src, Origin::FirstParty, true));
            added = true;
        }
        if added {
            self.match_order = match_order(&self.roots);
        }
    }

    /// The root with id `id`.
    pub fn root(&self, id: RootId) -> &ImportRoot {
        &self.roots[id as usize]
    }

    /// The [`RootKind::SitePackages`] root of a site index.
    pub fn site_root(&self, site: usize) -> Option<RootId> {
        self.roots.iter().position(|r| r.site == Some(site)).map(|i| i as RootId)
    }

    /// The root, origin, relative path and distribution of an absolute file path: the longest
    /// root containing it wins (so `src/` beats the project root and site-packages beats an
    /// enclosing project root), except that an editable-hook root giving the same module names
    /// as an enclosing first-party root yields to it. O(roots) prefix compares plus one hash lookup for site-packages
    /// files. `None` outside every root.
    pub fn classify<'p>(&self, path: &'p Path) -> Option<Located<'p>> {
        let p = path_bytes(path);
        for &id in &self.match_order {
            let r = &self.roots[id as usize];
            let rb = path_bytes(&r.path);
            if !p.starts_with(rb) {
                continue;
            }
            let (rel, single) = if p.len() == rb.len() {
                if !r.is_file {
                    continue;
                }
                (path.file_name().map_or(p, |n| n.as_bytes()), true)
            } else if r.is_file {
                continue;
            } else if rb.ends_with(b"/") {
                (&p[rb.len()..], false)
            } else if p[rb.len()] == b'/' {
                (&p[rb.len() + 1..], false)
            } else {
                continue;
            };
            let dist = match r.site {
                Some(s) => self.dists.owner(s, rel),
                None => r.dist,
            };
            return Some(Located { root: id, origin: r.origin, rel, single, dist });
        }
        None
    }

    /// Adds `path` to a module map builder (classifying it); returns the file index.
    pub fn add_file(&self, b: &mut ModuleMapBuilder, path: &Path) -> u32 {
        match self.classify(path) {
            Some(l) => b.add(l.root, &self.roots[l.root as usize].prefix, l.rel, l.single),
            None => b.add_outside(),
        }
    }

    /// Adds the files `dist` owns ([`Distributions::owned_files`], in `RECORD` order) to a
    /// module map builder; returns the index of the first one. Files are classified like
    /// [`Roots::add_file`], so a file under a `.pth` / hook root inside site-packages (pywin32's
    /// `win32/lib`, `foo.egg`) gets the same module name either way. Without such roots no path
    /// is built.
    pub fn add_record_files(&self, b: &mut ModuleMapBuilder, dist: DistId) -> u32 {
        let first = b.len() as u32;
        let Some(root) = self.site_root(self.dists.site(dist)) else {
            for _ in self.dists.owned_files(dist) {
                b.add_outside();
            }
            return first;
        };
        let site_path = &self.roots[root as usize].path;
        let nested = self.roots.iter().any(|r| r.site.is_none() && r.path != *site_path && is_under(&r.path, site_path));
        let mut buf = Vec::new();
        for rel in self.dists.owned_files(dist) {
            if !nested {
                b.add(root, b"", rel, false);
                continue;
            }
            buf.clear();
            buf.extend_from_slice(path_bytes(site_path));
            buf.push(b'/');
            buf.extend_from_slice(rel);
            self.add_file(b, bytes_path(&buf));
        }
        first
    }

    /// Module names and statuses for walked files (file index = position in `files`).
    pub fn module_map(&self, files: &PathList) -> ModuleMap {
        let mut b = ModuleMapBuilder::new();
        for p in files.iter() {
            self.add_file(&mut b, p);
        }
        b.finish()
    }

    /// What to load for analysis: the first-party `paths` walked with `first_party`, then
    /// (per the toggles) the stdlib, every third-party distribution with a `RECORD`, the
    /// site-packages entries no `RECORD` claims, and third-party `.pth` / editable roots outside
    /// site-packages.
    pub fn sources(&self, paths: &[PathBuf], first_party: &WalkOptions) -> Vec<Source> {
        let mut out = Vec::new();
        for p in paths {
            out.push(Source::Walk { path: p.clone(), opts: first_party.clone(), origin: Origin::FirstParty });
        }
        for r in self.roots.iter().filter(|r| r.analyse && r.origin != Origin::FirstParty) {
            match r.kind {
                RootKind::Stdlib => {
                    // The base interpreter's site-packages is not stdlib.
                    let opts = WalkOptions { excludes: vec!["/site-packages".to_owned()], ..WalkOptions::everything() };
                    out.push(Source::Walk { path: r.path.clone(), opts, origin: Origin::Stdlib });
                }
                RootKind::SitePackages => {
                    let Some(site) = r.site else { continue };
                    for d in self.dists.ids() {
                        if self.dists.site(d) == site && self.dists.owned_files(d).next().is_some() {
                            out.push(Source::Record { dist: d });
                        }
                    }
                    for path in self.unclaimed(site, &r.path) {
                        out.push(Source::Walk { path, opts: WalkOptions::everything(), origin: Origin::ThirdParty });
                    }
                }
                RootKind::Pth | RootKind::EditableHook => {
                    let inside_site = self.roots.iter().any(|s| s.kind == RootKind::SitePackages && is_under(&r.path, &s.path));
                    if !inside_site {
                        out.push(Source::Walk { path: r.path.clone(), opts: WalkOptions::default(), origin: r.origin });
                    }
                }
                RootKind::Configured | RootKind::Src | RootKind::Project => {}
            }
        }
        out
    }

    /// Top-level entries of a site-packages directory that no `RECORD` lists anything under
    /// (distributions without a file list, stray modules), sorted. One directory listing.
    pub fn unclaimed(&self, site: usize, site_path: &Path) -> Vec<PathBuf> {
        let mut out = Vec::new();
        let Ok(rd) = std::fs::read_dir(site_path) else { return out };
        for ent in rd.flatten() {
            let name = ent.file_name();
            let n = name.as_bytes();
            if n.starts_with(b".")
                || n.starts_with(b"__editable__")
                || n == b"__pycache__"
                || n.ends_with(b".dist-info")
                || n.ends_with(b".egg-info")
                || self.dists.is_claimed(site, n)
            {
                continue;
            }
            let Ok(ft) = ent.file_type() else { continue };
            if ft.is_dir() || (ft.is_file() && crate::walk::is_python(n)) {
                out.push(ent.path());
            }
        }
        out.sort_unstable();
        out
    }
}
