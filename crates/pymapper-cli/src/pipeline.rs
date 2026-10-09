//! The load pipeline shared by the subcommands that analyse a project:
//!
//! ```text
//!   Config ──> import roots (discover)      phase `roots`
//!          ──> walk: first-party walk,      phase `walk`
//!              stdlib walk, third-party RECORDs (+ walk of unclaimed site-packages entries)
//!          ──> project units (+ their import roots)
//!          ──> Inputs (module names, groups)
//!          ──> Index, through the cache     phases `load.*`, `cache.write`
//!          ──> import resolution            phases `resolve.*`
//! ```
//!
//! Nothing here formats per file or keeps sources: files are streamed through the extractor
//! by the index loader; this module only builds the path list and the inputs.

use std::io::{self, Write};
use std::path::{Path, PathBuf};

use pymapper_core::analysis::dupes::{DupesParams, Fingerprints};
use pymapper_core::cache::{self, Cache, CacheStats};
use pymapper_core::config::Config;
use pymapper_core::extract::config::ConfigEvidence;
use pymapper_core::extract::facts::sup_kind;
use pymapper_core::index::{self, Index, Inputs, LoadOptions};
use pymapper_core::resolve::{Attribution, Imports, resolve_imports};
use pymapper_core::roots::{self, ProjectUnits, RootOptions, Roots, Source};
use pymapper_core::rules::{self, BadCodeKind, Suppressions, SuppressionsBuilder};
use pymapper_core::timing;
use pymapper_core::walk::{self, PathList, WalkOptions};

/// Sets the size of the global worker pool (`threads`, 0 = one per logical CPU). Call once,
/// before any parallel work.
pub fn configure_threads(threads: u32) -> anyhow::Result<()> {
    if threads > 0 {
        rayon::ThreadPoolBuilder::new()
            .num_threads(threads as usize)
            .build_global()
            .map_err(|e| anyhow::anyhow!("cannot start {threads} worker threads: {e}"))?;
    }
    Ok(())
}

/// Prints a warning on stderr. Write failures are ignored: stderr may be closed and there is
/// nowhere else to report.
pub fn warn(msg: std::fmt::Arguments<'_>) {
    let _ = writeln!(io::stderr().lock(), "pymapper: warning: {msg}");
}

/// Fails when a path to analyse does not exist: a mistyped or stale path must not pass as a
/// clean run. `explicit`: the paths came from the command line (else from the config file
/// `config_file`, or the default). Paths that exist but cannot be read are left to the walk,
/// which reports them as warnings.
pub fn require_paths(paths: &[PathBuf], explicit: bool, config_file: Option<&Path>) -> anyhow::Result<()> {
    for p in paths {
        let missing = match std::fs::symlink_metadata(p) {
            Ok(_) => false,
            Err(e) => matches!(e.kind(), io::ErrorKind::NotFound | io::ErrorKind::NotADirectory),
        };
        if !missing {
            continue;
        }
        match config_file {
            Some(file) if !explicit => {
                anyhow::bail!("path `{}` (from `paths` in {}) does not exist", p.display(), file.display())
            }
            _ => anyhow::bail!("path `{}` does not exist", p.display()),
        }
    }
    Ok(())
}

/// The loaded project.
pub struct Project {
    /// The project root and the directories below it with a `pyproject.toml` (found by the
    /// first-party walk).
    pub units: ProjectUnits,
    pub index: Index,
    pub imports: Imports,
    /// Duplication fingerprints of first-party files, when requested.
    pub fingerprints: Option<Fingerprints>,
    /// What the cache did (`None`: cache disabled).
    pub cache: Option<CacheStats>,
    /// Non-fatal walk problems (unreadable directories, invalid excludes).
    pub walk_errors: usize,
    /// Dead-code evidence from config files (`analysis.config-evidence`).
    pub config_evidence: Option<ConfigEvidence>,
    /// The import roots and distributions the project was loaded with.
    pub roots: Roots,
}

/// Loads the project described by `config` (rooted at `project_root`).
///
/// `dupes`: take duplication fingerprints during extraction (see
/// [`DupesParams::from_engine`]).
pub fn load(config: &Config, project_root: &Path, dupes: Option<DupesParams>) -> Project {
    let mut roots = timing::time("roots", || roots::discover(project_root, &RootOptions::from_config(config)));
    for d in &roots.diagnostics {
        warn(format_args!("{d}"));
    }
    let (files, pyprojects, mut configs, walk_errors) = timing::time("walk", || collect_files(&roots, config, project_root));
    if let Some(report) = &config.analysis.coverage {
        exclude_file(&mut configs, report);
    }
    let (units, diags) = ProjectUnits::discover(&roots.project_root, roots.pyproject.as_ref(), &pyprojects);
    drop(pyprojects);
    for d in &diags {
        warn(format_args!("{d}"));
    }
    // Nested units (uv workspace members, monorepo packages) are import roots of their own.
    roots.add_unit_roots(&units);
    let inputs = Inputs::from_roots(&roots, &files);
    drop(files);
    let opts = LoadOptions { dupes };
    let (index, cache, fingerprints) = match Cache::from_config(config) {
        Some(c) => {
            let (ix, cs, fps) = cache::load_with(inputs, Some(&roots), &c, &opts);
            if let Some(e) = &cs.write_error {
                warn(format_args!("cannot write the cache in {}: {e}", c.dir().display()));
            }
            (ix, Some(cs), fps)
        }
        None => {
            let (ix, fps) = index::load_with(inputs, &opts);
            (ix, None, fps)
        }
    };
    if !index.read_errors().is_empty() {
        let e = &index.read_errors()[0];
        warn(format_args!(
            "{} file(s) could not be read (first: {}: {})",
            index.read_errors().len(),
            index.file_path(e.file).display(),
            io::Error::from(e.kind)
        ));
    }
    let imports = resolve_imports(&index, &Attribution::from_roots(&roots));
    let config_evidence = (config.analysis.config_evidence && config.analysis.name_fallback)
        .then(|| timing::time("config-evidence", || cache::config_evidence(&configs, Cache::from_config(config).as_ref())));
    Project { units, index, imports, fingerprints, cache, walk_errors, config_evidence, roots }
}

/// Removes `file` from `paths`: the same path, or another path to the same file (symlinks,
/// hard links, `..`), compared by device and inode among the paths with the same file name.
/// Used to keep the coverage report (`analysis.coverage`) out of the config-evidence files,
/// whatever its contents: it names every function, executed or not.
fn exclude_file(paths: &mut PathList, file: &Path) {
    use std::os::unix::fs::MetadataExt;
    let id = std::fs::metadata(file).ok().map(|m| (m.dev(), m.ino()));
    let name = file.file_name();
    paths.remove_if(|p| {
        if p == file {
            return true;
        }
        let Some(id) = id else { return false };
        p.file_name() == name && std::fs::metadata(p).is_ok_and(|m| (m.dev(), m.ino()) == id)
    });
}

/// Walk options equal for grouping purposes (walks with equal options share one walk).
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

/// Every file to load, in a deterministic order: walks of the sources (consecutive walks with
/// the same options merged into one walk), and the `RECORD` files of third-party
/// distributions. Returns the files, the `pyproject.toml` files and config files of the
/// first-party walks and the number of walk diagnostics (each printed).
pub fn collect_files(roots: &Roots, config: &Config, project_root: &Path) -> (PathList, PathList, PathList, usize) {
    let first_party = WalkOptions::first_party(config, project_root);
    let mut files = PathList::default();
    let mut projects = PathList::default();
    let mut configs = PathList::default();
    let mut errors = 0;
    let mut full = false;
    // Pending walk: roots sharing one set of options.
    let mut pending: Vec<PathBuf> = Vec::new();
    let mut pending_opts: Option<WalkOptions> = None;
    let mut buf = Vec::new();
    for src in roots.sources(&config.paths, &first_party) {
        match src {
            Source::Walk { path, opts, .. } => {
                if !pending_opts.as_ref().is_some_and(|p| same_opts(p, &opts)) {
                    flush_walk(&mut pending, pending_opts.as_ref(), &mut files, &mut projects, &mut configs, &mut errors, &mut full);
                    pending_opts = Some(opts);
                }
                pending.push(path);
            }
            Source::Record { dist } => {
                flush_walk(&mut pending, pending_opts.as_ref(), &mut files, &mut projects, &mut configs, &mut errors, &mut full);
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
    flush_walk(&mut pending, pending_opts.as_ref(), &mut files, &mut projects, &mut configs, &mut errors, &mut full);
    if full {
        warn(format_args!("more than 4 GiB of file paths: the remaining files were dropped"));
        errors += 1;
    }
    (files, projects, configs, errors)
}

/// Walks the pending roots (if any) with `opts`, appends the files to `files` (setting `full`
/// when the list overflows) and the `pyproject.toml` files to `projects`, and prints the
/// diagnostics.
fn flush_walk(
    pending: &mut Vec<PathBuf>,
    opts: Option<&WalkOptions>,
    files: &mut PathList,
    projects: &mut PathList,
    configs: &mut PathList,
    errors: &mut usize,
    full: &mut bool,
) {
    let Some(opts) = opts else { return };
    if pending.is_empty() {
        return;
    }
    let w = walk::walk(pending, opts);
    for d in &w.diagnostics {
        warn(format_args!("{d}"));
    }
    *errors += w.diagnostics.len();
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
    for i in 0..w.configs.len() {
        if configs.push(w.configs.get_bytes(i)).is_err() {
            break;
        }
    }
    pending.clear();
}

/// Inline `# pymapper: ignore[...]` and F401 `# noqa` suppressions of every file, from the facts.
/// Malformed codes suppress nothing; they, unknown codes and empty lists (`ignore[]`) are passed
/// to `bad(file, line, code, kind)` (`code` is empty for an empty list), in file and line order.
pub fn suppressions(ix: &Index, mut bad: impl FnMut(u32, u32, &[u8], BadCodeKind)) -> Suppressions {
    let mut b = SuppressionsBuilder::new();
    let st = &ix.facts().suppressions;
    let mut entries = Vec::new();
    for f in 0..ix.file_count() {
        let range = ix.file_facts(f).suppressions.range();
        if range.is_empty() {
            continue;
        }
        entries.clear();
        let view = ix.file_view(f);
        for r in range {
            let line = st.line[r];
            if st.kind[r] == sup_kind::NOQA_F401 {
                entries.push(rules::noqa_f401(line));
                continue;
            }
            if st.kind[r] == sup_kind::ALL {
                rules::parse_codes(line, None, &mut entries, |_| {});
            }
            let codes = view.chain(st.codes[r]);
            if st.kind[r] == sup_kind::CODES && codes.is_empty() {
                bad(f, line, b"", BadCodeKind::Malformed);
            }
            for &s in codes {
                let code = ix.str(s);
                rules::parse_codes(line, Some(code), &mut entries, |e| bad(f, line, code, e.kind));
            }
        }
        b.push_file(rules::FileId(f), &entries);
    }
    b.build()
}
