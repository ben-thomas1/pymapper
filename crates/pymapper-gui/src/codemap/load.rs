//! In-process analysis through core: the same load pipeline as the CLI's `check` (config,
//! import roots, walks, index through the cache, import and call resolution), then the
//! rollup base tables and the node aggregates the map needs.
//!
//! ```text
//!   config::load ─> roots::discover ─> walks + RECORDs ─> Inputs ─> Index (cache)
//!                ─> resolve_imports ─> resolve_calls ─> Rollup::build, Aggregates::build
//! ```
//!
//! The walk step mirrors `pymapper-cli/src/pipeline.rs` (the GUI cannot depend on the CLI
//! crate); warnings are collected for the UI instead of printed.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use pymapper_core::cache::{self, Cache};
use pymapper_core::incremental::{IndexDiff, Program, UpdateStats};
use pymapper_core::config::{self, Config, Overrides};
use pymapper_core::graph::CallFilter;
use pymapper_core::index::{self, Index, Inputs, LoadOptions};
use pymapper_core::resolve::{Attribution, Imports, ResolvedCalls, resolve_calls, resolve_imports};
use pymapper_core::rollup::{Aggregates, Rollup};
use pymapper_core::roots::{self, ProjectUnits, RootOptions, Roots, Source};
use pymapper_core::walk::{self, PathList, WalkOptions};
use pymapper_core::watch::{self, Changes};

/// What to load.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LoadParams {
    /// Project directory (config and project-root discovery start here).
    pub path: PathBuf,
    /// Ignore config files.
    pub isolated: bool,
    /// Include third-party distributions (`Some`: override the config).
    pub deps: Option<bool>,
    /// Include the stdlib (`Some`: override the config).
    pub stdlib: Option<bool>,
    /// Override the cache directory.
    pub cache_dir: Option<PathBuf>,
    /// Disable the cache.
    pub no_cache: bool,
    /// Resolve with dependency tracking so [`Analysis::update`] can apply file changes
    /// incrementally (watch mode).
    pub watch: bool,
}

/// The resolved program: plain, or tracked for incremental updates. One per analysis, so
/// the size difference of the variants (both a few KiB inline) does not matter.
#[allow(clippy::large_enum_variant)]
enum Resolved {
    Plain { index: Index, imports: Imports, calls: ResolvedCalls },
    Tracked(Program),
}

/// What a rewalk needs (watch mode): the configuration and the import roots of the load.
struct WalkCtx {
    config: Config,
    roots: Roots,
}

/// A loaded, resolved project with the tables the map draws from.
pub struct Analysis {
    pub root: PathBuf,
    resolved: Resolved,
    walk: Option<WalkCtx>,
    pub rollup: Rollup,
    pub agg: Aggregates,
    /// Wall time per phase, in order.
    pub phases: Vec<(&'static str, Duration)>,
    /// Non-fatal problems (config notes, unreadable files and directories).
    pub warnings: Vec<String>,
    /// Directory for the layout cache (`None`: cache disabled).
    pub cache_dir: Option<PathBuf>,
}

impl std::fmt::Debug for Analysis {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Analysis").field("root", &self.root).field("nodes", &self.index().node_count()).finish_non_exhaustive()
    }
}

impl Analysis {
    #[inline]
    pub fn index(&self) -> &Index {
        match &self.resolved {
            Resolved::Plain { index, .. } => index,
            Resolved::Tracked(p) => p.index(),
        }
    }

    #[inline]
    pub fn imports(&self) -> &Imports {
        match &self.resolved {
            Resolved::Plain { imports, .. } => imports,
            Resolved::Tracked(p) => p.imports(),
        }
    }

    #[inline]
    pub fn calls(&self) -> &ResolvedCalls {
        match &self.resolved {
            Resolved::Plain { calls, .. } => calls,
            Resolved::Tracked(p) => p.calls().expect("watch mode resolves calls"),
        }
    }

    /// Loaded for incremental updates ([`LoadParams::watch`]).
    pub fn watchable(&self) -> bool {
        matches!(self.resolved, Resolved::Tracked(_))
    }

    /// Applies a classified batch of file changes (watch mode; `changes.reload` is the
    /// caller's business): walks again when the file set may have changed, updates the
    /// program incrementally, then rebuilds the rollup and the aggregates (O(nodes + edges)).
    /// Walk problems are appended to `warnings`.
    ///
    /// # Panics
    /// If the analysis was not loaded with [`LoadParams::watch`].
    pub fn update(&mut self, changes: &Changes<'_>, warnings: &mut Vec<String>) -> (IndexDiff, UpdateStats, Duration) {
        let Resolved::Tracked(program) = &mut self.resolved else { panic!("Analysis::update needs a watch-mode load") };
        let ctx = self.walk.as_ref().expect("watch mode keeps the walk context");
        let root = &self.root;
        let (diff, st) = watch::update(program, changes, || {
            let (files, _) = collect_files(&ctx.roots, &ctx.config, root, warnings);
            Inputs::from_roots(&ctx.roots, &files)
        });
        let t = Instant::now();
        let (ix, imp) = (program.index(), program.imports());
        self.rollup = Rollup::build(ix, imp, program.calls().expect("watch mode resolves calls"), CallFilter::default(), 0);
        self.agg = Aggregates::build(ix);
        (diff, st, t.elapsed())
    }
}

/// Loads and resolves the project at `p.path`. Errors are configuration errors; unreadable
/// files are warnings.
pub fn load(p: &LoadParams, progress: &dyn Fn(&'static str)) -> anyhow::Result<Analysis> {
    let mut phases = Vec::new();
    let mut t = Instant::now();
    let mut lap = |name: &'static str, phases: &mut Vec<(&'static str, Duration)>| {
        let now = Instant::now();
        phases.push((name, now - t));
        t = now;
    };
    progress("config");
    let cwd = std::env::current_dir()?;
    let overrides = Overrides {
        isolated: p.isolated,
        paths: vec![p.path.clone()],
        include_deps: p.deps,
        include_stdlib: p.stdlib,
        cache_dir: p.cache_dir.clone(),
        cache: p.no_cache.then_some(false),
        ..Overrides::default()
    };
    let loaded = config::load(&cwd, &overrides)?;
    let mut warnings = loaded.warnings;
    let config = loaded.config;
    let root = loaded.project.root;
    lap("config", &mut phases);

    progress("walk");
    let mut roots = roots::discover(&root, &RootOptions::from_config(&config));
    warnings.extend(roots.diagnostics.iter().map(ToString::to_string));
    let (files, pyprojects) = collect_files(&roots, &config, &root, &mut warnings);
    let (units, diags) = ProjectUnits::discover(&roots.project_root, roots.pyproject.as_ref(), &pyprojects);
    drop(pyprojects);
    warnings.extend(diags.iter().map(ToString::to_string));
    roots.add_unit_roots(&units);
    let inputs = Inputs::from_roots(&roots, &files);
    drop(files);
    lap("walk", &mut phases);

    progress("index");
    let opts = LoadOptions::default();
    let cache = Cache::from_config(&config);
    let index = match &cache {
        Some(c) => {
            let (ix, cs, _) = cache::load_with(inputs, Some(&roots), c, &opts);
            if let Some(e) = &cs.write_error {
                warnings.push(format!("cannot write the cache in {}: {e}", c.dir().display()));
            }
            ix
        }
        None => index::load_with(inputs, &opts).0,
    };
    if let Some(e) = index.read_errors().first() {
        warnings.push(format!(
            "{} file(s) could not be read (first: {}: {})",
            index.read_errors().len(),
            index.file_path(e.file).display(),
            std::io::Error::from(e.kind)
        ));
    }
    lap("index", &mut phases);

    progress("resolve");
    let attr = Attribution::from_roots(&roots);
    let resolved = if p.watch {
        // Imports and calls with dependency tracking, in one step.
        let program = Program::from_parts(index, attr, true);
        lap("resolve (tracked)", &mut phases);
        Resolved::Tracked(program)
    } else {
        let imports = resolve_imports(&index, &attr);
        lap("resolve imports", &mut phases);
        let calls = resolve_calls(&index, &imports);
        lap("resolve calls", &mut phases);
        Resolved::Plain { index, imports, calls }
    };

    progress("rollup");
    let mut a = Analysis {
        root,
        resolved,
        walk: None,
        rollup: Rollup::default(),
        agg: Aggregates::default(),
        phases: Vec::new(),
        warnings,
        cache_dir: cache.map(|c| c.dir().to_path_buf()),
    };
    a.rollup = Rollup::build(a.index(), a.imports(), a.calls(), CallFilter::default(), 0);
    a.agg = Aggregates::build(a.index());
    lap("rollup + aggregates", &mut phases);
    a.phases = phases;
    if p.watch {
        a.walk = Some(WalkCtx { config, roots });
    }
    Ok(a)
}

/// Walk options equal for grouping purposes (as the CLI pipeline).
fn same_opts(a: &WalkOptions, b: &WalkOptions) -> bool {
    a.gitignore == b.gitignore
        && a.parents == b.parents
        && a.hidden == b.hidden
        && a.sort == b.sort
        && a.excludes == b.excludes
        && a.exclude_base == b.exclude_base
        && a.projects == b.projects
        && a.configs == b.configs
}

/// Every file to load and the first-party `pyproject.toml` files, as the CLI pipeline's
/// `collect_files`.
fn collect_files(roots: &Roots, config: &Config, project_root: &Path, warnings: &mut Vec<String>) -> (PathList, PathList) {
    let first_party = WalkOptions::first_party(config, project_root);
    let mut files = PathList::default();
    let mut projects = PathList::default();
    let mut full = false;
    let mut pending: Vec<PathBuf> = Vec::new();
    let mut pending_opts: Option<WalkOptions> = None;
    let mut buf = Vec::new();
    for src in roots.sources(&config.paths, &first_party) {
        match src {
            Source::Walk { path, opts, .. } => {
                if !pending_opts.as_ref().is_some_and(|p| same_opts(p, &opts)) {
                    flush(&mut pending, pending_opts.as_ref(), &mut files, &mut projects, warnings, &mut full);
                    pending_opts = Some(opts);
                }
                pending.push(path);
            }
            Source::Record { dist } => {
                flush(&mut pending, pending_opts.as_ref(), &mut files, &mut projects, warnings, &mut full);
                let site = roots.dists.site_path(dist).as_os_str().as_encoded_bytes();
                for rel in roots.dists.owned_files(dist) {
                    buf.clear();
                    buf.extend_from_slice(site);
                    buf.push(b'/');
                    buf.extend_from_slice(rel);
                    if files.push(&buf).is_err() {
                        full = true;
                        break;
                    }
                }
            }
        }
    }
    flush(&mut pending, pending_opts.as_ref(), &mut files, &mut projects, warnings, &mut full);
    if full {
        warnings.push("more than 4 GiB of file paths: the remaining files were dropped".to_owned());
    }
    (files, projects)
}

fn flush(pending: &mut Vec<PathBuf>, opts: Option<&WalkOptions>, files: &mut PathList, projects: &mut PathList, warnings: &mut Vec<String>, full: &mut bool) {
    let Some(opts) = opts else { return };
    if pending.is_empty() {
        return;
    }
    let w = walk::walk(pending, opts);
    warnings.extend(w.diagnostics.iter().map(ToString::to_string));
    for i in 0..w.files.len() {
        if files.push(w.files.get_bytes(i)).is_err() {
            *full = true;
            break;
        }
    }
    for i in 0..w.projects.len() {
        if projects.push(w.projects.get_bytes(i)).is_err() {
            break;
        }
    }
    pending.clear();
}
