//! The load pipeline shared by the subcommands that analyse a project:
//!
//! ```text
//!   Config ──> import roots (discover)      phase `roots`
//!          ──> files: first-party walk,     phase `files`
//!              stdlib walk, third-party RECORDs (+ walk of unclaimed site-packages entries)
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
use pymapper_core::index::{self, Index, Inputs, LoadOptions};
use pymapper_core::resolve::{Attribution, Imports, resolve_imports};
use pymapper_core::roots::{self, RootOptions, Roots, Source};
use pymapper_core::rules::{self, Suppressions, SuppressionsBuilder};
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

/// The loaded project.
pub struct Project {
    pub roots: Roots,
    pub index: Index,
    pub imports: Imports,
    /// Duplication fingerprints of first-party files, when requested.
    pub fingerprints: Option<Fingerprints>,
    /// What the cache did (`None`: cache disabled).
    pub cache: Option<CacheStats>,
    /// Non-fatal walk problems (unreadable directories, invalid excludes).
    pub walk_errors: usize,
}

/// Loads the project described by `config` (rooted at `project_root`).
///
/// `dupes`: take duplication fingerprints during extraction (see
/// [`DupesParams::from_engine`]).
pub fn load(config: &Config, project_root: &Path, dupes: Option<DupesParams>) -> Project {
    let roots = timing::time("roots", || roots::discover(project_root, &RootOptions::from_config(config)));
    for d in &roots.diagnostics {
        warn(format_args!("{d}"));
    }
    let (files, walk_errors) = timing::time("files", || collect_files(&roots, config, project_root));
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
    Project { roots, index, imports, fingerprints, cache, walk_errors }
}

/// Walk options equal for grouping purposes (walks with equal options share one walk).
fn same_opts(a: &WalkOptions, b: &WalkOptions) -> bool {
    a.gitignore == b.gitignore
        && a.parents == b.parents
        && a.hidden == b.hidden
        && a.sort == b.sort
        && a.excludes == b.excludes
        && a.exclude_base == b.exclude_base
}

/// Every file to load, in a deterministic order: walks of the sources (consecutive walks with
/// the same options merged into one walk), and the `RECORD` files of third-party
/// distributions. Returns the files and the number of walk diagnostics (each printed).
pub fn collect_files(roots: &Roots, config: &Config, project_root: &Path) -> (PathList, usize) {
    let first_party = WalkOptions::first_party(config, project_root);
    let mut files = PathList::default();
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
                    flush_walk(&mut pending, pending_opts.as_ref(), &mut files, &mut errors, &mut full);
                    pending_opts = Some(opts);
                }
                pending.push(path);
            }
            Source::Record { dist } => {
                flush_walk(&mut pending, pending_opts.as_ref(), &mut files, &mut errors, &mut full);
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
    flush_walk(&mut pending, pending_opts.as_ref(), &mut files, &mut errors, &mut full);
    if full {
        warn(format_args!("more than 4 GiB of file paths: the remaining files were dropped"));
        errors += 1;
    }
    (files, errors)
}

/// Walks the pending roots (if any) with `opts`, appends the files to `files` (setting `full`
/// when the list overflows) and prints the diagnostics.
fn flush_walk(pending: &mut Vec<PathBuf>, opts: Option<&WalkOptions>, files: &mut PathList, errors: &mut usize, full: &mut bool) {
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
    pending.clear();
}

/// Inline `# pymapper: ignore[...]` suppressions of every file, from the facts. Malformed codes
/// are skipped (they suppress nothing).
pub fn suppressions(ix: &Index) -> Suppressions {
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
            if st.all[r] != 0 {
                rules::parse_codes(st.line[r], None, &mut entries, |_| {});
            }
            for &s in view.chain(st.codes[r]) {
                rules::parse_codes(st.line[r], Some(ix.str(s)), &mut entries, |_| {});
            }
        }
        b.push_file(rules::FileId(f), &entries);
    }
    b.build()
}
